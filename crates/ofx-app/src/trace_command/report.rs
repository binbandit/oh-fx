use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use ofx_agent::CompactionEvent;
use ofx_contract::{PermissionMode, ReasoningEffort, is_provider_search_alias};
use ofx_text::mask_secrets;
use ofx_trace::Sequenced;

use crate::context::civil_from_unix_days;

const LINE_LIMIT: usize = 300;
const PROBLEM_DETAIL_LIMIT: usize = 160;
const COMPACTION_PROBLEMS: usize = 3;
const COMPACTION_EVENTS: usize = 24;
const TAIL_BYTES: u64 = 6 * 1024;
const TAIL_LINES: usize = 80;
const LOG_VARIABLE: &str = "OH_FX_TRACE_LOG";
const FLAG_VARIABLE: &str = "OH_FX_TRACE";
const SEARCH_NAME: &str = "web_search";
const NO_PROBLEMS: &str = "- no obvious errors captured in recent network, tool, compaction, MCP, or model catalog state\n";

pub(crate) struct TraceFacts {
    pub(crate) model: String,
    pub(crate) fast_mode: bool,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) workspace_root: PathBuf,
    pub(crate) step_limit: u64,
    pub(crate) effort: ReasoningEffort,
    pub(crate) processing: bool,
    pub(crate) stream_active: bool,
    pub(crate) queued: usize,
}

pub(super) struct Snapshot {
    facts: TraceFacts,
    generated_ms: i64,
    process: Process,
    trace_flag: bool,
    trace_log: Option<PathBuf>,
    terminal: Terminal,
    compaction: Vec<Sequenced<CompactionEvent>>,
    tail: Option<Tail>,
}

struct Process {
    pid: u32,
    open_fds: Option<usize>,
    memory: Option<String>,
}

struct Terminal {
    term: Option<String>,
    term_program: Option<String>,
    lang: Option<String>,
    tmux: bool,
    cmux: bool,
}

struct Tail {
    path: PathBuf,
    bytes: Vec<u8>,
    older_left_out: bool,
}

struct Timestamp(i64);

impl Snapshot {
    pub(super) fn capture(facts: TraceFacts) -> Self {
        let trace_log = ofx_trace::active_log_path()
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(LOG_VARIABLE).map(PathBuf::from));
        let tail = trace_log.as_deref().and_then(read_tail);
        let variable =
            |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
        let pid = std::process::id();
        Self {
            facts,
            generated_ms: ofx_trace::timestamp_ms(),
            process: Process {
                pid,
                open_fds: open_file_descriptors(),
                memory: process_memory(pid),
            },
            trace_flag: ofx_trace::is_truthy(std::env::var_os(FLAG_VARIABLE).as_deref()),
            trace_log,
            terminal: Terminal {
                term: variable("TERM"),
                term_program: variable("TERM_PROGRAM"),
                lang: variable("LANG"),
                tmux: std::env::var_os("TMUX").is_some(),
                cmux: std::env::var_os("CMUX_WORKSPACE_ID").is_some(),
            },
            compaction: ofx_agent::compaction_trace(),
            tail,
        }
    }

    pub(super) fn render(&self) -> String {
        let mut out = String::new();
        let _ = self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) -> fmt::Result {
        out.push_str("# oh-fx trace\n\n");
        out.push_str("Private diagnostic report. It may include prompts, file paths, command output, and file snippets.\n\n");
        self.write_summary(out)?;
        self.write_current_state(out)?;
        self.write_problems(out)?;
        self.write_compaction(out)?;
        self.write_runtime_context(out)?;
        if let Some(tail) = &self.tail {
            write_tail(out, tail)?;
        }
        Ok(())
    }

    fn write_summary(&self, out: &mut String) -> fmt::Result {
        let facts = &self.facts;
        out.push_str("## Summary\n");
        if self.generated_ms >= 0 {
            writeln!(out, "generated: {}", utc_seconds(self.generated_ms / 1000))?;
        }
        writeln!(out, "version: {}", ofx_upgrade::VERSION)?;
        writeln!(
            out,
            "platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )?;
        writeln!(
            out,
            "build: {}",
            if cfg!(debug_assertions) {
                "Debug"
            } else {
                "Release"
            }
        )?;
        writeln!(out, "model: {}", facts.model)?;
        if facts.fast_mode {
            out.push_str("fast_mode: on\n");
        }
        writeln!(out, "permission_mode: {}", facts.permission_mode.label())?;
        writeln!(out, "workspace: {}", facts.workspace_root.display())
    }

    fn write_current_state(&self, out: &mut String) -> fmt::Result {
        out.push_str("\n## Current State\n");
        writeln!(out, "agent_step_limit: {}", self.facts.step_limit)?;
        writeln!(out, "effort: {}", self.facts.effort.label())?;
        write!(out, "process: pid={}", self.process.pid)?;
        if let Some(count) = self.process.open_fds {
            write!(out, " open_fds={count}")?;
        }
        out.push('\n');
        if let Some(memory) = &self.process.memory {
            out.push_str("process_memory:\n");
            for line in memory.lines() {
                writeln!(out, "  {}", line.trim_end_matches([' ', '\t', '\r']))?;
            }
        }
        writeln!(
            out,
            "OH_FX_TRACE: {}",
            if self.trace_flag { "on" } else { "off" }
        )?;
        if let Some(path) = &self.trace_log {
            writeln!(out, "trace_log: {}", mask_secrets(&path.to_string_lossy()))?;
        }
        Ok(())
    }

    fn write_problems(&self, out: &mut String) -> fmt::Result {
        out.push_str("\n## Problems\n");
        let mut count = 0;
        if self.facts.processing || self.facts.stream_active {
            count += 1;
            writeln!(
                out,
                "- report captured an active turn; state may be partial processing={} stream_active={} queued={}",
                self.facts.processing, self.facts.stream_active, self.facts.queued
            )?;
        }
        for event in self
            .compaction
            .iter()
            .rev()
            .filter(|event| event.event.failed)
            .take(COMPACTION_PROBLEMS)
        {
            count += 1;
            let event = &event.event;
            write!(out, "- context compaction {}", event.kind.name())?;
            if event.context.turn_id != 0 {
                write!(out, " turn_id={}", event.context.turn_id)?;
            }
            if !event.detail.is_empty() {
                let visible = cut(&event.detail, PROBLEM_DETAIL_LIMIT);
                write!(out, " detail={}", neutralized(visible))?;
                if visible.len() < event.detail.len() {
                    out.push_str(" ...");
                }
            }
            out.push('\n');
        }
        if count == 0 {
            out.push_str(NO_PROBLEMS);
        }
        Ok(())
    }

    fn write_compaction(&self, out: &mut String) -> fmt::Result {
        out.push_str("\n## Context Compaction\n");
        let Some(first) = self.compaction.first() else {
            out.push_str("(none recorded)\n");
            return Ok(());
        };
        let total = self.compaction.len();
        let failed = self
            .compaction
            .iter()
            .filter(|event| event.event.failed)
            .count();
        write!(out, "last={total} failed={failed}")?;
        let overwritten = first.sequence.saturating_sub(1);
        if overwritten > 0 {
            write!(out, " overwritten_before={overwritten}")?;
        }
        out.push_str(" (always recorded; does not require OH_FX_TRACE)\n");
        let start = total.saturating_sub(COMPACTION_EVENTS);
        if start > 0 {
            writeln!(out, "... ({start} older events omitted)")?;
        }
        for event in &self.compaction[start..] {
            let event = &event.event;
            let mut line = format!(
                "{} event={}",
                Timestamp(event.timestamp_ms),
                event.kind.name()
            );
            if event.context.turn_id != 0 {
                write!(line, " turn_id={}", event.context.turn_id)?;
            }
            if event.context.step_id != 0 {
                write!(line, " step_id={}", event.context.step_id)?;
            }
            if event.context.subagent_id != 0 {
                write!(line, " subagent_id={}", event.context.subagent_id)?;
            }
            if event.failed {
                line.push_str(" failed");
            }
            if !event.detail.is_empty() {
                line.push(' ');
                line.push_str(&event.detail);
            }
            if event.truncated {
                line.push_str(" ...");
            }
            write_limited_line(out, &line);
        }
        Ok(())
    }

    fn write_runtime_context(&self, out: &mut String) -> fmt::Result {
        let terminal = &self.terminal;
        let shown = |value: &Option<String>| value.clone().unwrap_or_else(|| "(unset)".to_owned());
        out.push_str("\n## Runtime Context\n");
        writeln!(out, "TERM: {}", shown(&terminal.term))?;
        writeln!(out, "TERM_PROGRAM: {}", shown(&terminal.term_program))?;
        writeln!(out, "LANG: {}", shown(&terminal.lang))?;
        writeln!(
            out,
            "terminal_hosts: tmux={} cmux={}",
            terminal.tmux, terminal.cmux
        )
    }
}

pub(super) fn file_stamp(now_ms: i64) -> String {
    let seconds = now_ms.div_euclid(1000).max(0);
    let (year, month, day) = civil_from_unix_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year}-{month:02}-{day:02}-{:02}{:02}{:02}",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

fn utc_seconds(seconds: i64) -> String {
    let (year, month, day) = civil_from_unix_days(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 <= 0 {
            return formatter.write_str("[----------T--:--:--Z]");
        }
        let seconds = self.0 / 1000;
        let (year, month, day) = civil_from_unix_days(seconds / 86_400);
        let of_day = seconds % 86_400;
        write!(
            formatter,
            "[{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z]",
            of_day / 3600,
            of_day % 3600 / 60,
            of_day % 60,
            self.0 % 1000
        )
    }
}

fn write_tail(out: &mut String, tail: &Tail) -> fmt::Result {
    write!(
        out,
        "\n## Trace Tail\npath={} last_bytes={}\n",
        tail.path.display(),
        tail.bytes.len()
    )?;
    out.push_str("only obvious secrets masked\n");
    if tail.older_left_out {
        out.push_str("... (older lines truncated)\n");
    }
    let text = String::from_utf8_lossy(&tail.bytes);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .filter(|line| !line.is_empty())
        .collect();
    for line in &lines[lines.len().saturating_sub(TAIL_LINES)..] {
        write_limited_line(out, &mask_secrets(line));
    }
    Ok(())
}

fn write_limited_line(out: &mut String, line: &str) {
    let visible = cut(line, LINE_LIMIT);
    out.push_str(&neutralized(visible));
    if visible.len() < line.len() {
        out.push_str(" ...\n");
    } else {
        out.push('\n');
    }
}

fn cut(text: &str, limit: usize) -> &str {
    &text[..text.floor_char_boundary(limit)]
}

fn neutralized(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut written = 0;
    let mut index = 0;
    while index < bytes.len() {
        if !is_token_byte(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_token_byte(bytes[index]) {
            index += 1;
        }
        if let Some(alias) = search_alias_prefix(&text[start..index]) {
            let replaced = out.get_or_insert_with(String::new);
            replaced.push_str(&text[written..start]);
            replaced.push_str(SEARCH_NAME);
            written = start + alias;
        }
    }
    match out {
        Some(mut replaced) => {
            replaced.push_str(&text[written..]);
            Cow::Owned(replaced)
        }
        None => Cow::Borrowed(text),
    }
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn search_alias_prefix(token: &str) -> Option<usize> {
    (1..=token.len()).find(|&end| {
        is_provider_search_alias(&token[..end])
            && (end == token.len() || token.as_bytes()[end] == b'_')
    })
}

fn read_tail(path: &Path) -> Option<Tail> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size == 0 {
        return None;
    }
    let length = size.min(TAIL_BYTES);
    let offset = size - length;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or_default());
    file.take(length).read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(Tail {
        path: path.to_path_buf(),
        bytes,
        older_left_out: offset > 0,
    })
}

fn open_file_descriptors() -> Option<usize> {
    let directory = match std::env::consts::OS {
        "linux" => "/proc/self/fd",
        "macos" => "/dev/fd",
        _ => return None,
    };
    Some(fs::read_dir(directory).ok()?.count())
}

fn process_memory(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args([
            OsStr::new("-o"),
            OsStr::new("pid,ppid,rss,vsz,etime,stat"),
            OsStr::new("-p"),
            OsStr::new(&pid.to_string()),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim_matches([' ', '\t', '\r', '\n']);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests;
