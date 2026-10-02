use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;

use super::Shell;
use crate::terminal::TerminalError;
use crate::terminal::ThemeQuery;

const MAX_INPUT_READS_PER_FACT_COLLECTION: usize = 32;
const INPUT_CHUNK_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, Default)]
struct Readiness {
    input: bool,
    input_closed: bool,
}

enum InputRead {
    Closed,
    Drained,
    StillReadable,
}

pub(super) enum Exit {
    Quit,
    Signal(i32),
}

impl Shell<'_> {
    pub(super) fn run(&mut self) -> Result<Option<i32>, TerminalError> {
        loop {
            match self.step() {
                Ok(None) => {}
                Ok(Some(Exit::Quit)) => return Ok(None),
                Ok(Some(Exit::Signal(signal))) => return Ok(Some(signal)),
                Err(error) => return self.pending_fatal_signal().map(Some).ok_or(error),
            }
        }
    }

    pub(super) fn pending_fatal_signal(&mut self) -> Option<i32> {
        let signal = self.signals.take().fatal?;
        self.terminal.restore_abnormally();
        Some(signal)
    }

    pub(super) fn step(&mut self) -> Result<Option<Exit>, TerminalError> {
        self.commit_frame()?;
        let now_ms = self.now_ms();
        let timeout = self
            .next_deadline_ms(now_ms)
            .map(|deadline| Duration::from_millis(u64::try_from(deadline - now_ms).unwrap_or(0)));
        let ready = self.poll(timeout)?;
        if let Some(signal) = self.collect_facts()? {
            return Ok(Some(Exit::Signal(signal)));
        }
        if self.should_exit {
            return Ok(Some(Exit::Quit));
        }
        if ready.input {
            match self.read_input()? {
                InputRead::Closed => return Ok(Some(Exit::Quit)),
                InputRead::StillReadable => return Ok(self.should_exit.then_some(Exit::Quit)),
                InputRead::Drained => {}
            }
        }
        if ready.input_closed && !ready.input {
            return Ok(Some(Exit::Quit));
        }
        self.flush_pending_input()?;
        Ok(self.should_exit.then_some(Exit::Quit))
    }

    fn collect_facts(&mut self) -> Result<Option<i32>, TerminalError> {
        let signals = self.signals.take();
        if let Some(signal) = signals.fatal {
            self.terminal.restore_abnormally();
            return Ok(Some(signal));
        }
        let now_ms = self.now_ms();
        if signals.continued {
            self.reclaim_after_external_stop()?;
        }
        if signals.resized {
            self.handle_resize_signal(now_ms);
        }
        self.apply_pending_resize(now_ms);
        self.drain_ui_events();
        self.settle_clipboard();
        if self.gestures.expire(now_ms) {
            self.mark_dirty();
        }
        if let Some(query) = self.input.take_theme_query(now_ms) {
            let written = match query {
                ThemeQuery::ResponseFence => self.terminal.request_theme_response_fence(),
                ThemeQuery::Background => self.terminal.request_theme_background(),
            };
            if written.is_err() {
                self.input.fail_theme_query();
            }
        }
        if let Some(update) = self.input.take_theme_update() {
            self.apply_theme(update.light);
        }
        Ok(None)
    }

    fn poll(&self, timeout: Option<Duration>) -> Result<Readiness, TerminalError> {
        let timeout = timeout.and_then(|timeout| Timespec::try_from(timeout).ok());
        let clipboard = self.clipboard.fd();
        let mut fds = [
            PollFd::from_borrowed_fd(self.terminal.input_fd(), PollFlags::IN),
            PollFd::from_borrowed_fd(self.events.fd(), PollFlags::IN),
            PollFd::from_borrowed_fd(self.signals.fd(), PollFlags::IN),
            PollFd::from_borrowed_fd(clipboard.unwrap_or(self.signals.fd()), PollFlags::IN),
        ];
        let watched = if clipboard.is_some() {
            fds.len()
        } else {
            fds.len() - 1
        };
        match rustix::event::poll(&mut fds[..watched], timeout.as_ref()) {
            Ok(_) => {}
            Err(Errno::INTR) => return Ok(Readiness::default()),
            Err(errno) => return Err(errno.into()),
        }
        let input = fds[0].revents();
        Ok(Readiness {
            input: input.contains(PollFlags::IN),
            input_closed: input.intersects(PollFlags::HUP | PollFlags::ERR),
        })
    }

    fn read_input(&mut self) -> Result<InputRead, TerminalError> {
        let mut buffer = [0_u8; INPUT_CHUNK_BYTES];
        for _ in 0..MAX_INPUT_READS_PER_FACT_COLLECTION {
            let count = self.terminal.read(&mut buffer)?;
            if count == 0 {
                return Ok(InputRead::Closed);
            }
            self.input.push_bytes(&buffer[..count]);
            self.process_input()?;
            if self.should_exit {
                return Ok(InputRead::Drained);
            }
            let poll = self.terminal.poll_input(Some(Duration::ZERO))?;
            if !poll.readable {
                return Ok(InputRead::Drained);
            }
        }
        Ok(InputRead::StillReadable)
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{TurnId, TurnOutcome, UiCommand, UiEvent};

    use super::super::test_shell::TestShell;

    #[test]
    fn agent_facts_from_the_same_wake_land_before_typed_input() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.deliver(UiEvent::AssistantText {
            turn_id: TurnId::new(1),
            text: "First answer.".to_owned(),
        });
        test.submit("two");
        test.screen();
        test.queue(UiEvent::TurnFinished {
            turn_id: TurnId::new(1),
            outcome: TurnOutcome::Completed,
        });
        test.type_bytes(b"\x03");
        test.step();
        let screen = test.screen();
        assert!(screen.contains("First answer."), "{screen}");
        assert!(screen.contains("■ Cancelled"), "{screen}");
        assert_eq!(test.sent().len(), 2);
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(2),
        });
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::Cancel {
                turn_id: TurnId::new(2)
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_paste_end_marker_at_the_read_budget_waits_for_the_rest_of_the_input() {
        let mut test = TestShell::start();
        let mut clipboard = b"\x1b[200~".to_vec();
        clipboard.extend(std::iter::repeat_n(b'a', 4084));
        clipboard.extend_from_slice(b"\x1b[201~INJn\r\x1b[201~");
        test.type_bytes(&clipboard);
        test.step();
        test.step();
        assert!(test.sent().is_empty(), "{:?}", test.sent());
        assert!(test.shell.composer.is_empty());
        let screen = test.screen();
        assert!(
            screen.contains("Paste was not applied because extra input followed its end marker."),
            "{screen}"
        );
    }

    #[test]
    fn cancelling_a_started_turn_names_it() {
        let mut test = TestShell::start();
        test.submit("one");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(7),
        });
        test.type_bytes(b"\x03");
        test.step();
        assert_eq!(
            test.sent().last(),
            Some(&UiCommand::Cancel {
                turn_id: TurnId::new(7)
            })
        );
    }
}
