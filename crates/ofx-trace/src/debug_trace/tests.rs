use std::path::Path;

use super::*;

fn traced(options: Options) -> Trace {
    Trace::open(options).unwrap()
}

fn file_options(path: &Path) -> Options {
    Options {
        file_path: Some(path.to_path_buf()),
        ..Options::default()
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn is_truthy_parses_accepted_and_rejected_values() {
    let truthy = |raw: &str| is_truthy(Some(OsStr::new(raw)));
    assert!(truthy("1"));
    assert!(truthy("true"));
    assert!(truthy("YES"));
    assert!(truthy("on"));
    assert!(!truthy("0"));
    assert!(!truthy(""));
    assert!(!truthy(" \t\r\n"));
    assert!(!is_truthy(None));
}

#[test]
fn the_default_log_path_is_under_the_state_logs_directory() {
    assert_eq!(
        default_log_path(Some(Path::new("/tmp/fake-state/oh-fx")), 12_345),
        Path::new("/tmp/fake-state/oh-fx/logs/trace.log")
    );
}

#[test]
fn the_fallback_default_log_path_uses_a_tmp_trace_path() {
    assert_eq!(
        default_log_path(None, 12_345),
        Path::new("/tmp/oh-fx-trace-12345.log")
    );
}

#[test]
fn the_trace_logger_writes_the_configured_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.log");
    let trace = traced(file_options(&path));
    trace.line("test", format_args!("hello {}", 42));
    assert!(read(&path).contains("[test] hello 42"));
}

#[test]
fn the_trace_logger_filters_scopes_and_writes_structured_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scoped-trace.log");
    let trace = traced(Options {
        scope_filter: Some("agent, tool".to_owned()),
        ..file_options(&path)
    });
    trace.line("agent", format_args!("human line"));
    trace.line("worker", format_args!("filtered line"));
    trace.event(
        "tool",
        "execution_start",
        TraceContext {
            turn_id: 7,
            step_id: 11,
            subagent_id: 0,
        },
        Some(format_args!("name={}", "read_file")),
    );
    let written = read(&path);
    assert!(written.contains("[agent] human line"));
    assert!(!written.contains("[worker] filtered line"));
    assert!(written.contains("[tool] event=execution_start turn_id=7 step_id=11 name=read_file"));
}

#[test]
fn an_event_without_a_message_or_ids_ends_at_its_name() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.log");
    traced(file_options(&path)).event("tool", "done", TraceContext::default(), None);
    assert!(read(&path).ends_with(" [tool] event=done\n"));
}

#[test]
fn trace_ids_increase_from_one_call_to_the_next() {
    let first = next_turn_id();
    assert_eq!(next_turn_id(), first + 1);
    let step = next_step_id();
    assert_eq!(next_step_id(), step + 1);
    let subagent = next_subagent_id();
    assert_eq!(next_subagent_id(), subagent + 1);
}

#[test]
fn trace_lines_encode_every_non_ascii_and_control_byte() {
    assert_eq!(
        terminal_safe_line(b"server=bad\n\x1b]0;owned\x07\xff"),
        "server=bad\\x0a\\x1b]0;owned\\x07\\xff"
    );
}

#[test]
fn trace_lines_stop_at_their_limit_with_a_marker() {
    let line = terminal_safe_line(&vec![b'a'; MAX_TRACE_LINE_BYTES + 10]);
    assert_eq!(line.len(), MAX_TRACE_LINE_BYTES);
    assert!(line.ends_with("aaa..."));
    let encoded = terminal_safe_line(&vec![0xff; MAX_TRACE_LINE_BYTES]);
    assert!(encoded.len() <= MAX_TRACE_LINE_BYTES);
    assert!(encoded.ends_with("\\xff..."));
}

#[test]
fn resolve_log_path_resolves_absolute_and_relative_paths() {
    let workspace = Path::new("/tmp/workspace");
    assert_eq!(
        resolve_log_path(workspace, OsStr::new(" \t/tmp/oh-fx-absolute-trace.log\n")),
        Ok(PathBuf::from("/tmp/oh-fx-absolute-trace.log"))
    );
    assert_eq!(
        resolve_log_path(workspace, OsStr::new("logs/trace.log")),
        Ok(workspace.join("logs/trace.log"))
    );
}

#[test]
fn resolve_log_path_rejects_empty_trace_paths() {
    let workspace = Path::new("/tmp/workspace");
    assert_eq!(
        resolve_log_path(workspace, OsStr::new("")),
        Err(InvalidTracePath)
    );
    assert_eq!(
        resolve_log_path(workspace, OsStr::new(" \t\r\n")),
        Err(InvalidTracePath)
    );
}

#[test]
fn the_first_configuration_wins() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.log");
    let second = directory.path().join("second.log");
    let cell = OnceLock::new();
    configure_into(&cell, file_options(&first));
    configure_into(&cell, file_options(&second));
    let trace = cell.get().and_then(Option::as_ref).unwrap();
    trace.line("test", format_args!("first sink only"));
    assert!(read(&first).contains("first sink only"));
    assert!(!second.exists());
}

#[test]
fn a_configuration_that_cannot_open_its_file_leaves_tracing_off() {
    let directory = tempfile::tempdir().unwrap();
    let blocked = directory.path().join("file");
    fs::write(&blocked, "").unwrap();
    let cell = OnceLock::new();
    configure_into(
        &cell,
        Options {
            stderr_enabled: true,
            ..file_options(&blocked.join("trace.log"))
        },
    );
    assert!(cell.get().is_some_and(Option::is_none));
}

#[test]
fn configure_enables_stderr_only_tracing() {
    let trace = traced(Options {
        stderr_enabled: true,
        ..Options::default()
    });
    assert!(trace.allows("agent"));
    assert!(!traced(Options::default()).allows("agent"));
}

#[test]
fn the_environment_leaves_tracing_disabled_when_unset() {
    let options = options_from(|_| None, Path::new("/tmp/workspace"), None, 1).unwrap();
    assert_eq!(options, Options::default());
}

#[test]
fn the_environment_names_the_log_stderr_and_scopes() {
    let variables = [
        ("OH_FX_TRACE_LOG", "logs/trace.log"),
        ("OH_FX_TRACE_STDERR", "yes"),
        ("OH_FX_TRACE_SCOPES", "agent,tool"),
    ];
    let lookup = |name: &str| {
        variables
            .iter()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| OsString::from(value))
    };
    let options = options_from(lookup, Path::new("/tmp/workspace"), None, 1).unwrap();
    assert_eq!(
        options,
        Options {
            file_path: Some(PathBuf::from("/tmp/workspace/logs/trace.log")),
            stderr_enabled: true,
            scope_filter: Some("agent,tool".to_owned()),
        }
    );
    let flag = |name: &str| (name == "OH_FX_TRACE").then(|| OsString::from("1"));
    let state = Path::new("/home/ada/.local/state/oh-fx");
    assert_eq!(
        options_from(flag, Path::new("/tmp/workspace"), Some(state), 1)
            .unwrap()
            .file_path,
        Some(state.join("logs/trace.log"))
    );
    let empty = |name: &str| (name == "OH_FX_TRACE_LOG").then(|| OsString::from(" "));
    assert_eq!(
        options_from(empty, Path::new("/tmp/workspace"), None, 1),
        Err(InvalidTracePath)
    );
}

#[test]
fn a_log_over_its_limit_is_emptied_when_tracing_starts() {
    let directory = tempfile::tempdir().unwrap();
    let large = directory.path().join("large.log");
    let small = directory.path().join("small.log");
    fs::write(&large, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    fs::write(&small, "kept\n").unwrap();
    traced(file_options(&large));
    traced(file_options(&small));
    assert_eq!(fs::metadata(&large).unwrap().len(), 0);
    assert_eq!(read(&small), "kept\n");
}

#[test]
fn the_log_directory_is_created_when_tracing_starts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("logs/nested/trace.log");
    traced(file_options(&path)).line("test", format_args!("created"));
    assert!(read(&path).contains("[test] created"));
}
