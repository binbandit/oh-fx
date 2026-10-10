use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use super::*;

struct RecordingClipboard {
    copied: Mutex<Vec<String>>,
    succeeds: bool,
}

impl RecordingClipboard {
    fn new(succeeds: bool) -> Self {
        Self {
            copied: Mutex::new(Vec::new()),
            succeeds,
        }
    }
}

impl Clipboard for RecordingClipboard {
    fn copy(&self, text: &str) -> bool {
        self.copied.lock().unwrap().push(text.to_owned());
        self.succeeds
    }
}

fn saved_report(directory: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

#[test]
fn the_trace_notice_distinguishes_markdown_file_outcomes_without_a_feedback_cta() {
    let path = PathBuf::from("/tmp/trace.md");
    for (disposition, tone, expected) in [
        (
            Disposition::CopiedFile,
            NoticeTone::Neutral,
            "Trace copied to clipboard. Review and redact it before sharing.",
        ),
        (
            Disposition::CopyFailed(path.clone()),
            NoticeTone::Error,
            "Clipboard copy failed. Trace saved at /tmp/trace.md. Review and redact it before sharing.",
        ),
        (
            Disposition::Saved(path.clone()),
            NoticeTone::Neutral,
            "Trace saved at /tmp/trace.md. Review and redact it before sharing.",
        ),
        (
            Disposition::Unavailable,
            NoticeTone::Error,
            "Could not create trace.",
        ),
    ] {
        let notice = trace_notice(&disposition);
        assert_eq!(notice, Notice::new(tone, "", expected));
        assert!(!notice.body.contains("feedback"));
        assert!(!notice.body.contains("Report issue"));
    }
}

#[test]
fn the_trace_report_file_uses_a_private_randomized_markdown_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_report(directory.path(), "hello\n", 1_780_000_000_123).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("oh-fx-trace-2026-05-28-202640-"), "{name}");
    assert_eq!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("md")
    );
    assert_eq!(name.len(), "oh-fx-trace-2026-05-28-202640-".len() + 12 + 3);
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.len(), 6);
    assert_eq!(metadata.permissions().mode() & 0o077, 0);
    let other = write_report(directory.path(), "hello\n", 1_780_000_000_123).unwrap();
    assert_ne!(path, other);
}

#[test]
fn a_copied_report_is_also_saved_and_reports_the_clipboard() {
    let directory = tempfile::tempdir().unwrap();
    let clipboard = RecordingClipboard::new(true);
    let disposition = deliver("report\n", directory.path(), &clipboard, true);
    assert_eq!(disposition, Disposition::CopiedFile);
    assert_eq!(*clipboard.copied.lock().unwrap(), ["report\n"]);
    assert_eq!(
        fs::read_to_string(saved_report(directory.path())).unwrap(),
        "report\n"
    );
}

#[test]
fn a_failed_copy_names_the_saved_report_with_upstreams_platform_wording() {
    for (macos, failed_copy) in [(true, true), (false, false)] {
        let directory = tempfile::tempdir().unwrap();
        let clipboard = RecordingClipboard::new(false);
        let disposition = deliver("report\n", directory.path(), &clipboard, macos);
        let saved = saved_report(directory.path());
        let expected = if failed_copy {
            Disposition::CopyFailed(saved)
        } else {
            Disposition::Saved(saved)
        };
        assert_eq!(disposition, expected);
    }
}

#[test]
fn a_report_that_cannot_be_written_is_not_copied() {
    let directory = tempfile::tempdir().unwrap();
    let clipboard = RecordingClipboard::new(true);
    let missing = directory.path().join("missing");
    assert_eq!(
        deliver("report\n", &missing, &clipboard, true),
        Disposition::Unavailable
    );
    assert!(clipboard.copied.lock().unwrap().is_empty());
}
