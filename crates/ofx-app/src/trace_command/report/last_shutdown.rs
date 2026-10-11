use std::fmt::{self, Write as _};

use ofx_text::mask_secrets;
use serde_json::Value;

use super::utc_seconds;

pub(super) fn write_section(out: &mut String, contents: Option<&[u8]>) -> fmt::Result {
    out.push_str("\n## Last Shutdown\n");
    let Some(contents) = contents else {
        out.push_str("(none recorded)\n");
        return Ok(());
    };
    let Ok(Value::Object(report)) = serde_json::from_slice::<Value>(contents) else {
        out.push_str("(last shutdown report unreadable)\n");
        return Ok(());
    };
    let total = report.get("total_ms").and_then(Value::as_i64).unwrap_or(-1);
    write!(out, "total_ms={total}")?;
    if let Some(recorded) = report
        .get("recorded_at_ms")
        .and_then(Value::as_i64)
        .filter(|recorded| *recorded >= 0)
    {
        write!(out, " recorded={}", utc_seconds(recorded / 1000))?;
    }
    out.push('\n');
    let stages = report.get("stages").and_then(Value::as_array);
    for stage in stages.into_iter().flatten() {
        let name = stage.get("name").and_then(Value::as_str).unwrap_or("?");
        let step = stage.get("step_ms").and_then(Value::as_i64).unwrap_or(-1);
        writeln!(out, "- {} step_ms={step}", single_line(&mask_secrets(name)))?;
    }
    Ok(())
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
