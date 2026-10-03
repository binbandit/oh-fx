use std::env;
use std::path::{Path, PathBuf};

use ofx_contract::{Notice, NoticeTone, UiEvent};
use ofx_tui::{StartRecording, TerminalRecorder, UiEventSender};
use ofx_workspace::{CaptureStatus, TapeRecorder};

const RECORDING_TOPIC: &str = "recording";

struct Tape(TapeRecorder);

impl TerminalRecorder for Tape {
    fn stdout(&self, bytes: &[u8]) {
        self.0.record_stdout(bytes);
    }

    fn stdin(&self, bytes: &[u8]) {
        self.0.record_stdin(bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.0.record_resize(cols, rows);
    }

    fn stop(&self) {
        self.0.shutdown();
    }
}

pub(crate) fn start_recording(state: Option<PathBuf>, notices: UiEventSender) -> StartRecording {
    Box::new(move |cols, rows| {
        let lookup = |name: &str| env::var(name).ok();
        let started =
            TapeRecorder::start(&lookup, state.as_deref(), cols, rows, ofx_upgrade::VERSION)?;
        let Some(tape) = started else {
            return Ok(None);
        };
        if let CaptureStatus::Active {
            path,
            show_inline_notice: true,
        } = tape.capture_status()
        {
            notices.send(UiEvent::Notice {
                notice: recording_notice(&path),
            });
        }
        Ok(Some(Box::new(Tape(tape)) as Box<dyn TerminalRecorder>))
    })
}

fn recording_notice(path: &Path) -> Notice {
    Notice::new(
        NoticeTone::Warning,
        RECORDING_TOPIC,
        format!(
            "visual terminal capture: {}\nvisible terminal content, including typed prompt text, is recorded",
            path.display()
        ),
    )
}
