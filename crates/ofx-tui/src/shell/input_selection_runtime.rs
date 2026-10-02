use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;

use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;

use super::Shell;
use crate::composer::SelectionRange;
use crate::host::Clipboard;

const COPY_THREAD: &str = "oh-fx-clipboard";

struct Request {
    text: Arc<str>,
    cut: Option<SelectionRange>,
}

struct Running {
    request: Request,
    done: UnixStream,
}

pub(super) struct ClipboardRuntime {
    clipboard: Arc<dyn Clipboard>,
    running: Option<Running>,
    waiting: Option<Request>,
}

impl ClipboardRuntime {
    pub(super) fn new(clipboard: Arc<dyn Clipboard>) -> Self {
        Self {
            clipboard,
            running: None,
            waiting: None,
        }
    }

    pub(super) fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.running.as_ref().map(|running| running.done.as_fd())
    }

    fn request(&mut self, request: Request) {
        if self.running.is_some() {
            self.waiting = Some(request);
        } else {
            self.running = start(&self.clipboard, request);
        }
    }

    pub(super) fn finish(&mut self) {
        while let Some(done) = self.fd() {
            let mut fds = [PollFd::from_borrowed_fd(done, PollFlags::IN)];
            if let Err(error) = rustix::event::poll(&mut fds, None)
                && error != Errno::INTR
            {
                return;
            }
            self.take_finished();
        }
    }

    fn take_finished(&mut self) -> Option<(Request, bool)> {
        let running = self.running.as_mut()?;
        let mut outcome = [0_u8; 1];
        let copied = match running.done.read(&mut outcome) {
            Ok(count) => count == 1 && outcome[0] == 1,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                return None;
            }
            Err(_) => false,
        };
        let finished = self.running.take()?;
        self.running = self
            .waiting
            .take()
            .and_then(|request| start(&self.clipboard, request));
        Some((finished.request, copied))
    }
}

fn start(clipboard: &Arc<dyn Clipboard>, request: Request) -> Option<Running> {
    let (done, report) = UnixStream::pair().ok()?;
    done.set_nonblocking(true).ok()?;
    let clipboard = Arc::clone(clipboard);
    let text = Arc::clone(&request.text);
    thread::Builder::new()
        .name(COPY_THREAD.to_owned())
        .spawn(move || {
            let copied = clipboard.copy(&text);
            let _ = (&report).write(&[u8::from(copied)]);
        })
        .ok()?;
    Some(Running { request, done })
}

impl Shell<'_> {
    pub(super) fn copy_selection(&mut self) {
        if let Some(text) = self.composer.selected_text() {
            let text = text.into();
            self.clipboard.request(Request { text, cut: None });
        }
    }

    pub(super) fn cut_selection(&mut self) {
        if let (Some(range), Some(text)) =
            (self.composer.selection(), self.composer.selected_text())
        {
            let text = text.into();
            self.clipboard.request(Request {
                text,
                cut: Some(range),
            });
        }
    }

    pub(super) fn settle_clipboard(&mut self) {
        let Some((request, copied)) = self.clipboard.take_finished() else {
            return;
        };
        let Some(range) = request.cut else {
            return;
        };
        if copied
            && self.composer.selection() == Some(range)
            && self.composer.selected_text() == Some(&*request.text)
            && self.composer.delete_selection()
        {
            self.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::super::test_shell::TestShell;
    use crate::input::{
        COMPOSER_INPUT_LIMIT_BYTES, MoveIntent, MoveKind, PasteOutcome, PasteOwner,
    };

    const SELECT_ALL: &[u8] = b"\x1b[97;9u";
    const COPY: &[u8] = b"\x1b[99;9u";
    const CUT: &[u8] = b"\x1b[120;9u";
    const COPY_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[27;9;99~";
    const CUT_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[27;9;120~";
    const SELECT_WORD_LEFT: &[u8] = b"\x1b[1;4D";

    fn draft(fixture: &mut TestShell, text: &str) {
        fixture
            .shell
            .composer
            .insert_text(text, COMPOSER_INPUT_LIMIT_BYTES);
    }

    fn press(test: &mut TestShell, keys: &[u8]) {
        test.type_bytes(keys);
        test.draining(|shell| shell.step().unwrap());
    }

    fn settle(test: &mut TestShell) {
        while test.shell.clipboard.fd().is_some() {
            test.draining(|shell| shell.step().unwrap());
        }
    }

    fn move_cursor(test: &mut TestShell, kind: MoveKind, extend_selection: bool) {
        test.shell.composer.move_cursor(MoveIntent {
            kind,
            extend_selection,
        });
    }

    fn select_first_word(test: &mut TestShell) {
        move_cursor(test, MoveKind::DraftStart, false);
        move_cursor(test, MoveKind::WordRight, true);
    }

    #[test]
    fn selection_clipboard_operations_copy_before_cut() {
        let mut test = TestShell::start();
        draft(&mut test, "alpha beta");
        select_first_word(&mut test);

        test.shell.copy_selection();
        settle(&mut test);
        assert_eq!(test.copied(), ["alpha"]);
        assert_eq!(test.shell.composer.text(), "alpha beta");
        assert_eq!(test.shell.composer.selected_text(), Some("alpha"));

        test.shell.cut_selection();
        settle(&mut test);
        assert_eq!(test.copied(), ["alpha", "alpha"]);
        assert_eq!(test.shell.composer.text(), " beta");
        assert!(test.shell.composer.undo());
        assert_eq!(test.shell.composer.text(), "alpha beta");
    }

    #[test]
    fn failed_clipboard_copy_preserves_selection_and_text() {
        let mut test = TestShell::start();
        test.clipboard.fails.store(true, Ordering::Release);
        draft(&mut test, "alpha beta");
        select_first_word(&mut test);

        test.shell.cut_selection();
        settle(&mut test);
        assert_eq!(test.copied(), ["alpha"]);
        assert_eq!(test.shell.composer.text(), "alpha beta");
        assert_eq!(test.shell.composer.selected_text(), Some("alpha"));
    }

    #[test]
    fn copy_and_cut_without_a_selection_run_no_clipboard_command() {
        let mut test = TestShell::start();
        press(&mut test, b"keep me");
        press(&mut test, COPY);
        press(&mut test, CUT);
        assert!(test.shell.clipboard.fd().is_none());
        assert!(test.copied().is_empty());
        assert_eq!(test.shell.composer.text(), "keep me");
    }

    #[test]
    fn the_shortcuts_copy_the_selection_and_cut_removes_it() {
        let mut test = TestShell::start();
        press(&mut test, b"keep me");
        press(&mut test, SELECT_ALL);
        press(&mut test, COPY);
        settle(&mut test);
        press(&mut test, COPY_MODIFY_OTHER_KEYS);
        settle(&mut test);
        assert_eq!(test.copied(), ["keep me", "keep me"]);
        assert_eq!(test.shell.composer.text(), "keep me");

        move_cursor(&mut test, MoveKind::DraftEnd, false);
        press(&mut test, SELECT_WORD_LEFT);
        assert_eq!(test.shell.composer.selected_text(), Some("me"));
        press(&mut test, CUT);
        settle(&mut test);
        assert_eq!(test.shell.composer.text(), "keep ");

        press(&mut test, SELECT_ALL);
        press(&mut test, CUT_MODIFY_OTHER_KEYS);
        settle(&mut test);
        assert_eq!(test.copied(), ["keep me", "keep me", "me", "keep "]);
        assert_eq!(test.shell.composer.text(), "");
        assert!(!test.screen().contains("keep"));
    }

    #[test]
    fn keys_typed_while_the_command_runs_reach_the_draft_and_keep_the_cut_from_deleting() {
        let mut test = TestShell::start();
        press(&mut test, b"alpha beta");
        test.clipboard.hold();
        press(&mut test, SELECT_ALL);
        press(&mut test, CUT);
        press(&mut test, b"x");
        assert_eq!(test.shell.composer.text(), "x");
        assert!(test.screen().contains("┃ x"));
        test.clipboard.release();
        settle(&mut test);
        assert_eq!(test.copied(), ["alpha beta"]);
        assert_eq!(test.shell.composer.text(), "x");
    }

    #[test]
    fn a_cut_applies_when_the_same_selection_is_made_again_before_it_settles() {
        let mut test = TestShell::start();
        press(&mut test, b"alpha beta");
        test.clipboard.hold();
        press(&mut test, SELECT_ALL);
        press(&mut test, CUT);
        move_cursor(&mut test, MoveKind::CharacterLeft, false);
        assert_eq!(test.shell.composer.selection(), None);
        press(&mut test, SELECT_ALL);
        test.clipboard.release();
        settle(&mut test);
        assert_eq!(test.shell.composer.text(), "");
    }

    #[test]
    fn leaving_the_shell_finishes_the_copies_still_in_flight() {
        let mut test = TestShell::start();
        draft(&mut test, "alpha beta");
        test.clipboard.hold();
        press(&mut test, SELECT_ALL);
        press(&mut test, COPY);
        select_first_word(&mut test);
        press(&mut test, COPY);
        let clipboard = Arc::clone(&test.clipboard);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            clipboard.release();
        });
        test.shell.clipboard.finish();
        assert!(test.shell.clipboard.fd().is_none());
        assert_eq!(test.copied(), ["alpha beta", "alpha"]);
        release.join().unwrap();
    }

    #[test]
    fn a_request_made_while_one_runs_waits_and_the_latest_replaces_it() {
        let mut test = TestShell::start();
        draft(&mut test, "alpha beta");
        test.clipboard.hold();
        press(&mut test, SELECT_ALL);
        press(&mut test, COPY);
        select_first_word(&mut test);
        press(&mut test, COPY);
        move_cursor(&mut test, MoveKind::DraftEnd, false);
        press(&mut test, SELECT_WORD_LEFT);
        press(&mut test, CUT);
        test.clipboard.release();
        settle(&mut test);
        assert_eq!(test.copied(), ["alpha beta", "beta"]);
        assert_eq!(test.shell.composer.text(), "alpha ");
    }

    #[test]
    fn a_pasted_block_is_copied_as_its_placeholder_and_cut_with_its_text() {
        let mut test = TestShell::start();
        let pasted = "api_key=sk-live-0123456789\n".repeat(64);
        test.shell.handle_paste(PasteOutcome::Text {
            owner: PasteOwner::Composer,
            text: pasted.clone(),
        });
        let placeholder = test.shell.composer.text().to_owned();
        assert!(placeholder.starts_with("[Pasted text #1"));
        press(&mut test, SELECT_ALL);
        press(&mut test, COPY);
        settle(&mut test);
        assert_eq!(test.copied(), [placeholder.as_str()]);
        assert_eq!(test.shell.composer.expanded_text(), pasted);

        press(&mut test, CUT);
        settle(&mut test);
        assert_eq!(test.copied(), [placeholder.as_str(), placeholder.as_str()]);
        assert_eq!(test.shell.composer.expanded_text(), "");
        assert!(!test.screen().contains("sk-live"));
    }

    #[test]
    fn an_authorization_code_paste_never_reaches_the_clipboard() {
        let mut test = TestShell::start();
        press(&mut test, b"keep ");
        test.shell.input.begin_paste(PasteOwner::AuthCode, 64);
        press(&mut test, b"code-123\x1b[201~");
        press(&mut test, SELECT_ALL);
        press(&mut test, CUT);
        settle(&mut test);
        assert_eq!(test.copied(), ["keep "]);
        assert_eq!(test.shell.composer.text(), "");
        assert!(!test.screen().contains("code-123"));
    }

    #[test]
    fn a_copy_never_writes_the_selection_to_the_terminal() {
        let mut test = TestShell::start();
        let hostile = "keep \u{1b}]52;c;cm0gLXJmIH4=\u{7} me";
        draft(&mut test, hostile);
        press(&mut test, SELECT_ALL);
        let mut written = test.written();
        press(&mut test, COPY);
        settle(&mut test);
        press(&mut test, CUT);
        settle(&mut test);
        written.push_str(&test.written());
        assert_eq!(test.copied(), [hostile, hostile]);
        assert_eq!(test.shell.composer.text(), "");
        assert!(!written.contains("\u{1b}]52"), "{written:?}");
        assert!(!written.contains('\u{7}'), "{written:?}");
    }

    #[test]
    fn a_large_selection_is_copied_and_cut_whole() {
        let mut test = TestShell::start();
        let huge = "clipboard ✓\n".repeat(1024 * 1024 / "clipboard ✓\n".len());
        draft(&mut test, &huge);
        assert_eq!(test.shell.composer.text().len(), huge.len());
        press(&mut test, SELECT_ALL);
        press(&mut test, CUT);
        settle(&mut test);
        assert_eq!(test.copied(), [huge]);
        assert_eq!(test.shell.composer.text(), "");
    }

    #[test]
    fn a_failed_copy_reports_nothing_on_screen() {
        let mut test = TestShell::start();
        test.clipboard.fails.store(true, Ordering::Release);
        press(&mut test, b"keep me");
        press(&mut test, SELECT_ALL);
        let before = test.screen();
        press(&mut test, CUT);
        settle(&mut test);
        assert_eq!(test.copied(), ["keep me"]);
        assert_eq!(test.screen(), before);
        assert_eq!(test.shell.composer.selected_text(), Some("keep me"));
    }
}
