use std::fmt::{self, Write as _};

use ofx_text::mask_secrets;

use crate::app_session_runtime::{TitleSnapshot, TitleStatus};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionTitleFacts {
    pub(crate) setting: bool,
    pub(crate) title: Option<String>,
    pub(crate) generation: TitleSnapshot,
}

pub(super) fn write_problems(
    out: &mut String,
    facts: &SessionTitleFacts,
) -> Result<usize, fmt::Error> {
    let last = &facts.generation.last;
    if !matches!(last.status, TitleStatus::Failed | TitleStatus::Dropped) {
        return Ok(0);
    }
    write!(out, "- session title generation {}", last.status.name())?;
    if let Some(reason) = last.reason {
        write!(out, " reason={reason}")?;
    }
    if !last.detail.is_empty() {
        write!(out, " detail={}", last.detail)?;
    }
    out.push('\n');
    Ok(1)
}

pub(super) fn write_section(out: &mut String, facts: &SessionTitleFacts) -> fmt::Result {
    out.push_str("\n## Session Title\n");
    writeln!(out, "setting: {}", facts.setting)?;
    match &facts.title {
        Some(title) => writeln!(out, "title: {}", mask_secrets(title))?,
        None => out.push_str("title: (none)\n"),
    }
    if let Some(running) = &facts.generation.running {
        return writeln!(
            out,
            "generation: status=running session={} model={} elapsed={}ms",
            running.session, running.model, running.elapsed_ms
        );
    }
    let last = &facts.generation.last;
    write!(out, "generation: status={}", last.status.name())?;
    if !last.session.is_empty() {
        write!(out, " session={}", last.session)?;
    }
    if !last.model.is_empty() {
        write!(out, " model={}", last.model)?;
    }
    if let Some(reason) = last.reason {
        write!(out, " reason={reason}")?;
    }
    if !last.detail.is_empty() {
        write!(out, " detail={}", last.detail)?;
    }
    if let Some(elapsed) = last.elapsed_ms {
        write!(out, " elapsed={elapsed}ms")?;
    }
    out.push('\n');
    Ok(())
}

#[cfg(test)]
mod tests;
