use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use ofx_contract::{Notice, NoticeTone, UiEvent};
use ofx_tui::Clipboard;

use crate::app_agent_runtime::Emit;

mod report;

pub(crate) use report::TraceFacts;

const PATH_ATTEMPTS: usize = 8;
const RANDOM_BYTES: usize = 6;
const FALLBACK_DIRECTORY: &str = "/tmp";
const REVIEW: &str = "Review and redact it before sharing.";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Disposition {
    CopiedFile,
    CopyFailed(PathBuf),
    Saved(PathBuf),
    Unavailable,
}

pub(crate) fn start_trace(emit: &Emit, clipboard: Arc<dyn Clipboard>, facts: TraceFacts) {
    let worker_emit = Arc::clone(emit);
    let spawned = thread::Builder::new()
        .name("trace".to_owned())
        .spawn(move || {
            let report = report::Snapshot::capture(facts).render();
            let directory = std::env::var_os("TMPDIR")
                .filter(|directory| !directory.is_empty())
                .unwrap_or_else(|| OsString::from(FALLBACK_DIRECTORY));
            let disposition = deliver(
                &report,
                Path::new(&directory),
                &*clipboard,
                cfg!(target_os = "macos"),
            );
            worker_emit(UiEvent::Notice {
                notice: trace_notice(&disposition),
            });
        });
    if spawned.is_err() {
        emit(UiEvent::Notice {
            notice: trace_notice(&Disposition::Unavailable),
        });
    }
}

fn deliver(report: &str, directory: &Path, clipboard: &dyn Clipboard, macos: bool) -> Disposition {
    let Ok(path) = write_report(directory, report, ofx_trace::timestamp_ms()) else {
        return Disposition::Unavailable;
    };
    if clipboard.copy(report) {
        Disposition::CopiedFile
    } else if macos {
        Disposition::CopyFailed(path)
    } else {
        Disposition::Saved(path)
    }
}

fn write_report(directory: &Path, contents: &str, now_ms: i64) -> io::Result<PathBuf> {
    let stamp = report::file_stamp(now_ms);
    for _ in 0..PATH_ATTEMPTS {
        let mut random = [0_u8; RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|_| io::Error::from(io::ErrorKind::Unsupported))?;
        let mut name = format!("oh-fx-trace-{stamp}-");
        for byte in random {
            let _ = write!(name, "{byte:02x}");
        }
        name.push_str(".md");
        let path = directory.join(name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        return Ok(path);
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

fn trace_notice(disposition: &Disposition) -> Notice {
    let (tone, body) = match disposition {
        Disposition::CopiedFile => (
            NoticeTone::Neutral,
            format!("Trace copied to clipboard. {REVIEW}"),
        ),
        Disposition::CopyFailed(path) => (
            NoticeTone::Error,
            format!(
                "Clipboard copy failed. Trace saved at {}. {REVIEW}",
                path.display()
            ),
        ),
        Disposition::Saved(path) => (
            NoticeTone::Neutral,
            format!("Trace saved at {}. {REVIEW}", path.display()),
        ),
        Disposition::Unavailable => (NoticeTone::Error, "Could not create trace.".to_owned()),
    };
    Notice::new(tone, "", body)
}

#[cfg(test)]
mod tests;
