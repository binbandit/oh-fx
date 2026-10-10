use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TRACE_LINE_BYTES: usize = 64 * 1024;
const CUT_MARKER: &str = "...";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const LOG_VARIABLE: &str = "OH_FX_TRACE_LOG";
const FLAG_VARIABLE: &str = "OH_FX_TRACE";
const STDERR_VARIABLE: &str = "OH_FX_TRACE_STDERR";
const SCOPES_VARIABLE: &str = "OH_FX_TRACE_SCOPES";
const LOGS_DIRECTORY: &str = "logs";
const LOG_FILE: &str = "trace.log";

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STEP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SUBAGENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceContext {
    pub turn_id: u64,
    pub step_id: u64,
    pub subagent_id: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    file_path: Option<PathBuf>,
    stderr_enabled: bool,
    scope_filter: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InvalidTracePath;

struct Trace {
    stderr_enabled: bool,
    scope_filter: Option<String>,
    file_path: Option<PathBuf>,
    writing: Mutex<()>,
}

struct EventLine<'a> {
    name: &'a str,
    context: TraceContext,
    message: Option<fmt::Arguments<'a>>,
}

pub fn configure_from_env(workspace_root: &Path, state_directory: Option<&Path>) {
    let lookup = |name: &str| std::env::var_os(name);
    let Ok(options) = options_from(lookup, workspace_root, state_directory, timestamp_ms()) else {
        return;
    };
    if options.file_path.is_none() && !options.stderr_enabled {
        return;
    }
    configure_into(&TRACE, options);
}

pub fn enabled(scope: &str) -> bool {
    active().is_some_and(|trace| trace.allows(scope))
}

pub fn event(scope: &str, name: &str, context: TraceContext, message: Option<fmt::Arguments<'_>>) {
    if let Some(trace) = active() {
        trace.event(scope, name, context, message);
    }
}

pub fn log(scope: &str, message: fmt::Arguments<'_>) {
    if let Some(trace) = active() {
        trace.line(scope, message);
    }
}

pub fn active_log_path() -> Option<&'static Path> {
    active()?.file_path.as_deref()
}

pub fn next_turn_id() -> u64 {
    NEXT_TURN_ID.fetch_add(1, Ordering::SeqCst)
}

pub fn next_step_id() -> u64 {
    NEXT_STEP_ID.fetch_add(1, Ordering::SeqCst)
}

pub fn next_subagent_id() -> u64 {
    NEXT_SUBAGENT_ID.fetch_add(1, Ordering::SeqCst)
}

pub fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

pub fn is_truthy(value: Option<&OsStr>) -> bool {
    value
        .and_then(OsStr::to_str)
        .map(|raw| raw.trim_matches(TRIMMED))
        .is_some_and(|trimmed| {
            ["1", "true", "yes", "on"]
                .iter()
                .any(|accepted| trimmed.eq_ignore_ascii_case(accepted))
        })
}

fn active() -> Option<&'static Trace> {
    TRACE.get().and_then(Option::as_ref)
}

fn configure_into(cell: &OnceLock<Option<Trace>>, options: Options) {
    cell.get_or_init(|| Trace::open(options).ok());
}

fn options_from(
    lookup: impl Fn(&str) -> Option<OsString>,
    workspace_root: &Path,
    state_directory: Option<&Path>,
    now_ms: i64,
) -> Result<Options, InvalidTracePath> {
    let stderr_enabled = is_truthy(lookup(STDERR_VARIABLE).as_deref());
    let scope_filter = lookup(SCOPES_VARIABLE).and_then(|scopes| scopes.into_string().ok());
    let file_path = if let Some(raw) = lookup(LOG_VARIABLE) {
        Some(resolve_log_path(workspace_root, &raw)?)
    } else if is_truthy(lookup(FLAG_VARIABLE).as_deref()) {
        Some(default_log_path(state_directory, now_ms))
    } else {
        None
    };
    Ok(Options {
        file_path,
        stderr_enabled,
        scope_filter,
    })
}

fn resolve_log_path(workspace_root: &Path, raw: &OsStr) -> Result<PathBuf, InvalidTracePath> {
    let bytes = raw.as_bytes();
    let kept = |byte: &u8| !TRIMMED.contains(&char::from(*byte));
    let Some(start) = bytes.iter().position(kept) else {
        return Err(InvalidTracePath);
    };
    let end = bytes.iter().rposition(kept).map_or(start, |last| last + 1);
    let path = Path::new(OsStr::from_bytes(&bytes[start..end]));
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(workspace_root.join(path))
    }
}

fn default_log_path(state_directory: Option<&Path>, now_ms: i64) -> PathBuf {
    state_directory.map_or_else(
        || PathBuf::from(format!("/tmp/oh-fx-trace-{now_ms}.log")),
        |state| state.join(LOGS_DIRECTORY).join(LOG_FILE),
    )
}

fn rotate_if_too_large(path: &Path, max_bytes: u64) {
    let too_large = fs::metadata(path).is_ok_and(|metadata| metadata.len() > max_bytes);
    if too_large {
        let _ = File::create(path);
    }
}

fn append_line(path: &Path, line: &str) {
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if file.lock().is_ok() {
        let _ = file.write_all(line.as_bytes());
    }
}

fn terminal_safe_line(raw: &[u8]) -> String {
    let mut line = String::with_capacity(raw.len().min(MAX_TRACE_LINE_BYTES));
    for &byte in raw {
        let printable = (0x20..=0x7e).contains(&byte);
        let encoded = if printable { 1 } else { 4 };
        if line.len() + encoded + CUT_MARKER.len() > MAX_TRACE_LINE_BYTES {
            line.push_str(CUT_MARKER);
            return line;
        }
        if printable {
            line.push(char::from(byte));
        } else {
            let _ = write!(line, "\\x{byte:02x}");
        }
    }
    line
}

impl Trace {
    fn open(options: Options) -> io::Result<Self> {
        if let Some(path) = &options.file_path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            rotate_if_too_large(path, DEFAULT_LOG_MAX_BYTES);
            OpenOptions::new().create(true).append(true).open(path)?;
        }
        let scope_filter = options
            .scope_filter
            .map(|filter| filter.trim_matches(TRIMMED).to_owned())
            .filter(|filter| !filter.is_empty());
        Ok(Self {
            stderr_enabled: options.stderr_enabled,
            scope_filter,
            file_path: options.file_path,
            writing: Mutex::new(()),
        })
    }

    fn is_enabled(&self) -> bool {
        self.stderr_enabled || self.file_path.is_some()
    }

    fn allows(&self, scope: &str) -> bool {
        self.is_enabled()
            && self.scope_filter.as_deref().is_none_or(|filter| {
                filter
                    .split(',')
                    .map(|part| part.trim_matches(TRIMMED))
                    .any(|part| !part.is_empty() && part == scope)
            })
    }

    fn event(
        &self,
        scope: &str,
        name: &str,
        context: TraceContext,
        message: Option<fmt::Arguments<'_>>,
    ) {
        self.line(
            scope,
            format_args!(
                "{}",
                EventLine {
                    name,
                    context,
                    message
                }
            ),
        );
    }

    fn line(&self, scope: &str, message: fmt::Arguments<'_>) {
        if !self.allows(scope) {
            return;
        }
        let mut raw = String::new();
        if write!(raw, "{} [{scope}] {message}", timestamp_ms()).is_err() {
            return;
        }
        let mut line = terminal_safe_line(raw.as_bytes());
        line.push('\n');
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stderr_enabled {
            let _ = io::stderr().write_all(line.as_bytes());
        }
        if let Some(path) = &self.file_path {
            append_line(path, &line);
        }
    }
}

impl fmt::Display for EventLine<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "event={}", self.name)?;
        if self.context.turn_id != 0 {
            write!(formatter, " turn_id={}", self.context.turn_id)?;
        }
        if self.context.step_id != 0 {
            write!(formatter, " step_id={}", self.context.step_id)?;
        }
        if self.context.subagent_id != 0 {
            write!(formatter, " subagent_id={}", self.context.subagent_id)?;
        }
        if let Some(message) = self.message {
            write!(formatter, " {message}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
