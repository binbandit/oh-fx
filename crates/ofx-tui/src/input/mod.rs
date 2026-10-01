mod escape_parser;
pub(crate) mod gesture_state;
mod ingress_queue;
mod input_action;
mod paste_framing;
mod shortcuts;
mod terminal_action_decoder;
mod text_scalar;

use std::collections::VecDeque;

use crate::terminal::{ThemeMonitor, ThemeMonitorFeed, ThemeQuery, ThemeUpdate};

pub(crate) use input_action::{
    Action, DecodedTerminalAction, MoveIntent, MoveKind, RawTerminalInput, ShortcutAction,
};
pub(crate) use paste_framing::{COMPOSER_INPUT_LIMIT_BYTES, PasteOutcome, PasteOwner};
pub(crate) use text_scalar::{DroppedText, TextDropReason, TextOwner};

use ingress_queue::IngressQueue;
use input_action::{TerminalDecodeContext, TerminalInputEvent};
use paste_framing::PasteFraming;
use terminal_action_decoder::Decoder;

pub(crate) const INPUT_ESCAPE_TIMEOUT_MS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputContext {
    pub(crate) now_ms: i64,
    pub(crate) cancel_pending: bool,
    pub(crate) text_owner: TextOwner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputEvent {
    Action(DecodedTerminalAction),
    Raw(RawTerminalInput),
    Text(char),
    TextDropped(DroppedText),
    Paste(PasteOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Fresh,
    AfterThemeMonitor,
    Decoder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    ThemeMonitor,
    Paste,
    Input,
}

#[derive(Debug, Default)]
pub(crate) struct TerminalInput {
    theme_monitor: ThemeMonitor,
    decoder: Decoder,
    paste: PasteFraming,
    text: text_scalar::State,
    fresh: IngressQueue,
    staged: IngressQueue,
    replay: Option<u8>,
    events: VecDeque<InputEvent>,
}

impl TerminalInput {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        self.fresh.push(bytes);
    }

    pub(crate) fn next_event(&mut self, context: InputContext) -> Option<InputEvent> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Some(event);
            }
            let (entry, byte) = self.next_byte()?;
            self.dispatch(entry, byte, context);
        }
    }

    pub(crate) fn replay_byte(&mut self, byte: u8) {
        self.replay = Some(byte);
    }

    pub(crate) fn settle_delivery_epoch(&mut self) -> Option<InputEvent> {
        if !self.paste.active() || self.has_unclassified_input() {
            return None;
        }
        self.paste.settle_delivery_epoch().map(InputEvent::Paste)
    }

    pub(crate) fn flush_escape(&mut self, now_ms: i64) -> Option<InputEvent> {
        if self.has_queued_input() {
            return None;
        }
        let ingress = self
            .decoder
            .flush(now_ms, INPUT_ESCAPE_TIMEOUT_MS, self.paste.active());
        match ingress.event? {
            TerminalInputEvent::Action(decoded) => Some(InputEvent::Action(decoded)),
            TerminalInputEvent::Raw(raw) => Some(InputEvent::Raw(raw)),
            TerminalInputEvent::PasteByte(_) => None,
        }
    }

    pub(crate) fn begin_paste(&mut self, owner: PasteOwner, max_buffer_len: usize) {
        self.drop_pending_text(TextDropReason::PasteStarted);
        self.paste.begin(owner, max_buffer_len);
        self.decoder.reset();
    }

    pub(crate) fn has_pending_input(&self) -> bool {
        self.decoder.has_pending()
            || self.theme_monitor.has_pending_input()
            || !self.fresh.is_empty()
            || !self.staged.is_empty()
            || self.replay.is_some()
            || !self.events.is_empty()
    }

    pub(crate) fn start_theme_monitor(&mut self) {
        self.theme_monitor.start();
    }

    pub(crate) fn poll_theme_monitor(&mut self, now_ms: i64) {
        self.theme_monitor.poll(now_ms);
    }

    pub(crate) fn take_theme_query(&mut self, now_ms: i64) -> Option<ThemeQuery> {
        self.theme_monitor.take_query_request(now_ms)
    }

    pub(crate) fn fail_theme_query(&mut self) {
        self.theme_monitor.fail_query();
    }

    pub(crate) fn take_theme_update(&mut self) -> Option<ThemeUpdate> {
        self.theme_monitor.take_settled_update()
    }

    fn has_unclassified_input(&self) -> bool {
        self.has_queued_input() || self.theme_monitor.has_pending_input()
    }

    fn has_queued_input(&self) -> bool {
        !self.fresh.is_empty()
            || !self.staged.is_empty()
            || self.replay.is_some()
            || !self.events.is_empty()
            || self.theme_monitor.has_deferred_bytes()
    }

    fn next_byte(&mut self) -> Option<(Entry, u8)> {
        if let Some(byte) = self.replay.take() {
            return Some((Entry::Decoder, byte));
        }
        if let Some(byte) = self.staged.pop() {
            return Some((Entry::AfterThemeMonitor, byte));
        }
        if let Some(byte) = self.theme_monitor.take_deferred_byte() {
            return Some((Entry::AfterThemeMonitor, byte));
        }
        self.fresh.pop().map(|byte| (Entry::Fresh, byte))
    }

    fn dispatch(&mut self, entry: Entry, byte: u8, context: InputContext) {
        match entry {
            Entry::Fresh => match self.owner() {
                Owner::ThemeMonitor => self.feed_theme_monitor(byte, context),
                Owner::Paste => self.paste.consume_byte(byte),
                Owner::Input if self.theme_monitor.enabled => {
                    self.feed_theme_monitor(byte, context);
                }
                Owner::Input => self.after_theme_monitor(byte, context),
            },
            Entry::AfterThemeMonitor => self.after_theme_monitor(byte, context),
            Entry::Decoder => self.decode(byte, context),
        }
    }

    fn owner(&self) -> Owner {
        if self.theme_monitor.owns_input() {
            Owner::ThemeMonitor
        } else if self.paste.active() {
            Owner::Paste
        } else {
            Owner::Input
        }
    }

    fn feed_theme_monitor(&mut self, byte: u8, context: InputContext) {
        if let ThemeMonitorFeed::Forward(bytes) = self.theme_monitor.feed(byte, context.now_ms) {
            debug_assert!(self.staged.is_empty());
            self.staged.push(bytes.as_slice());
        }
    }

    fn after_theme_monitor(&mut self, byte: u8, context: InputContext) {
        if self.paste.active() {
            self.paste.consume_byte(byte);
            return;
        }
        self.decode(byte, context);
    }

    fn decode(&mut self, byte: u8, context: InputContext) {
        let ingress = self.decoder.feed(
            byte,
            TerminalDecodeContext {
                now_ms: context.now_ms,
                paste_active: self.paste.active(),
                cancel_pending: context.cancel_pending,
            },
        );
        if ingress.interrupts_pending_text {
            self.drop_pending_text(TextDropReason::InterruptedByNonTextInput);
        }
        if let Some(replay) = ingress.replay_byte_after_routing {
            self.replay = Some(replay);
        }
        match ingress.event {
            None => {}
            Some(TerminalInputEvent::PasteByte(byte)) => self.paste.consume_byte(byte),
            Some(TerminalInputEvent::Raw(raw)) if raw.byte >= 0x80 => {
                self.admit_text_byte(raw.byte, context.text_owner);
            }
            Some(TerminalInputEvent::Raw(raw)) => self.events.push_back(InputEvent::Raw(raw)),
            Some(TerminalInputEvent::Action(decoded)) => {
                self.events.push_back(InputEvent::Action(decoded));
            }
        }
    }

    fn admit_text_byte(&mut self, byte: u8, owner: TextOwner) {
        let transition = text_scalar::advance(self.text, owner, byte);
        self.text = transition.next;
        if let Some(dropped) = transition.step.dropped {
            self.events.push_back(InputEvent::TextDropped(dropped));
        }
        if let Some(scalar) = transition.step.scalar {
            self.events.push_back(InputEvent::Text(scalar));
        }
    }

    fn drop_pending_text(&mut self, reason: TextDropReason) {
        if let Some(dropped) = text_scalar::reset(self.text, reason) {
            self.events.push_back(InputEvent::TextDropped(dropped));
        }
        self.text = text_scalar::State::default();
    }
}

#[cfg(test)]
mod tests {
    use super::input_action::{MousePointer, MousePointerKind, MouseWheel};
    use super::*;

    fn context(now_ms: i64) -> InputContext {
        InputContext {
            now_ms,
            cancel_pending: false,
            text_owner: TextOwner::Composer,
        }
    }

    fn drain(input: &mut TerminalInput, now_ms: i64) -> Vec<InputEvent> {
        std::iter::from_fn(|| input.next_event(context(now_ms))).collect()
    }

    fn action(action: Action) -> InputEvent {
        InputEvent::Action(DecodedTerminalAction {
            action,
            composer_shortcut: ShortcutAction::from_escape_action(action),
            cancel_pending: false,
        })
    }

    fn raw(byte: u8) -> InputEvent {
        InputEvent::Raw(RawTerminalInput {
            byte,
            composer_shortcut: ShortcutAction::from_control_byte(byte),
        })
    }

    #[test]
    fn terminal_reply_ownership_precedes_active_paste_transport() {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        assert_eq!(input.take_theme_query(0), None);
        input.push_bytes(b"\x1b[?997;1n");
        assert!(drain(&mut input, 1).is_empty());
        assert_eq!(
            input.take_theme_query(1000),
            Some(ThemeQuery::ResponseFence)
        );

        input.begin_paste(PasteOwner::Composer, usize::MAX);
        assert_eq!(input.owner(), Owner::ThemeMonitor);
        input.push_bytes(b"\x1b[?1;2;4c");
        assert!(drain(&mut input, 1001).is_empty());
        assert_eq!(input.owner(), Owner::Paste);
    }

    #[test]
    fn plain_bytes_decode_to_raw_events_and_utf8_text() {
        let mut input = TerminalInput::new();
        input.push_bytes("a\u{e9}\x01".as_bytes());
        assert_eq!(
            drain(&mut input, 0),
            vec![raw(b'a'), InputEvent::Text('\u{e9}'), raw(1)]
        );
    }

    #[test]
    fn interrupted_text_reports_the_dropped_bytes() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\xe2\x82x");
        assert_eq!(
            drain(&mut input, 0),
            vec![
                InputEvent::TextDropped(DroppedText {
                    owner: TextOwner::Composer,
                    bytes: 2,
                    reason: TextDropReason::InterruptedByNonTextInput,
                }),
                raw(b'x'),
            ]
        );
    }

    #[test]
    fn kitty_and_modify_other_keys_reports_decode_to_actions() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[13;2u\x1b[27;5;111~\x1b[A");
        assert_eq!(
            drain(&mut input, 0),
            vec![
                action(Action::InsertNewline),
                action(Action::ToggleFullTranscript),
                action(Action::CursorUp),
            ]
        );
    }

    #[test]
    fn lone_escape_resolves_only_after_the_timeout() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b");
        assert!(drain(&mut input, 0).is_empty());
        assert!(input.has_pending_input());
        assert_eq!(input.flush_escape(INPUT_ESCAPE_TIMEOUT_MS - 1), None);
        assert_eq!(
            input.flush_escape(INPUT_ESCAPE_TIMEOUT_MS),
            Some(action(Action::Escape))
        );
        assert!(!input.has_pending_input());
    }

    #[test]
    fn flush_escape_waits_for_queued_undecoded_bytes() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b");
        assert!(drain(&mut input, 0).is_empty());
        input.push_bytes(b"[A");
        assert_eq!(input.flush_escape(INPUT_ESCAPE_TIMEOUT_MS + 1), None);
        assert_eq!(
            drain(&mut input, INPUT_ESCAPE_TIMEOUT_MS + 1),
            vec![action(Action::CursorUp)]
        );
    }

    #[test]
    fn settling_a_paste_waits_for_queued_trailing_input() {
        let mut input = TerminalInput::new();
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        input.push_bytes(b"safe\x1b[201~");
        assert!(drain(&mut input, 0).is_empty());
        input.push_bytes(b"\r");
        assert_eq!(input.settle_delivery_epoch(), None);
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(
            input.settle_delivery_epoch(),
            Some(InputEvent::Paste(PasteOutcome::TrailingInput {
                owner: PasteOwner::Composer
            }))
        );
    }

    #[test]
    fn escape_followed_by_a_control_byte_replays_it_after_routing() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b\x03");
        assert_eq!(drain(&mut input, 0), vec![action(Action::Escape), raw(3)]);
    }

    #[test]
    fn remapped_bytes_replay_through_the_decoder_only() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[99;5u");
        let events = drain(&mut input, 0);
        assert_eq!(events, vec![action(Action::RemappedByte(3))]);
        input.replay_byte(3);
        assert_eq!(drain(&mut input, 0), vec![raw(3)]);
    }

    #[test]
    fn bracketed_paste_captures_bytes_until_the_epoch_settles() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[200~");
        assert_eq!(drain(&mut input, 0), vec![action(Action::PasteStart)]);
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        input.push_bytes(b"one\r\n\x1b[Atwo\x1b[201~");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(
            input.settle_delivery_epoch(),
            Some(InputEvent::Paste(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "one\n[Atwo".to_owned(),
            }))
        );
        assert!(!input.paste.active());
    }

    #[test]
    fn paste_split_across_epochs_keeps_capturing() {
        let mut input = TerminalInput::new();
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        input.push_bytes(b"abc");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.settle_delivery_epoch(), None);
        assert!(input.paste.active());
        input.push_bytes(b"def\x1b[201~");
        drain(&mut input, 1);
        assert_eq!(
            input.settle_delivery_epoch(),
            Some(InputEvent::Paste(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "abcdef".to_owned(),
            }))
        );
    }

    #[test]
    fn input_after_the_paste_end_marker_in_one_epoch_rejects_the_paste() {
        let mut input = TerminalInput::new();
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        input.push_bytes(b"safe\x1b[201~\r");
        drain(&mut input, 0);
        assert_eq!(
            input.settle_delivery_epoch(),
            Some(InputEvent::Paste(PasteOutcome::TrailingInput {
                owner: PasteOwner::Composer
            }))
        );
    }

    fn paste_after_an_in_flight_theme_query(suffix: &[u8]) -> TerminalInput {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b[?997;1n");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.take_theme_query(0), Some(ThemeQuery::ResponseFence));
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        let mut read = b"safe\x1b[201~".to_vec();
        read.extend_from_slice(suffix);
        input.push_bytes(&read);
        assert!(drain(&mut input, 1).is_empty());
        input
    }

    fn trailing_input() -> InputEvent {
        InputEvent::Paste(PasteOutcome::TrailingInput {
            owner: PasteOwner::Composer,
        })
    }

    #[test]
    fn a_partial_theme_candidate_after_the_end_marker_holds_the_paste_until_it_resolves() {
        for (rest, decoded) in [(&b"13u"[..], "kitty enter"), (&b"A"[..], "cursor up")] {
            let mut input = paste_after_an_in_flight_theme_query(b"\x1b[");
            assert_eq!(input.settle_delivery_epoch(), None, "{decoded}");
            input.push_bytes(rest);
            assert!(drain(&mut input, 2).is_empty(), "{decoded}");
            assert_eq!(
                input.settle_delivery_epoch(),
                Some(trailing_input()),
                "{decoded}"
            );
            assert!(!input.has_pending_input(), "{decoded}");
        }
    }

    #[test]
    fn a_theme_reply_completing_after_the_end_marker_is_not_a_paste_suffix() {
        let mut input = paste_after_an_in_flight_theme_query(b"\x1b[?1;");
        assert_eq!(input.settle_delivery_epoch(), None);
        input.push_bytes(b"2c");
        assert!(drain(&mut input, 2).is_empty());
        assert_eq!(
            input.settle_delivery_epoch(),
            Some(InputEvent::Paste(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "safe".to_owned(),
            }))
        );
        assert_eq!(input.take_theme_query(3), Some(ThemeQuery::Background));
    }

    #[test]
    fn a_theme_candidate_that_never_completes_rejects_the_paste_after_the_idle_timeout() {
        let mut input = paste_after_an_in_flight_theme_query(b"\x1b[");
        assert_eq!(input.settle_delivery_epoch(), None);
        input.poll_theme_monitor(76);
        assert_eq!(input.settle_delivery_epoch(), None);
        assert!(drain(&mut input, 76).is_empty());
        assert_eq!(input.settle_delivery_epoch(), Some(trailing_input()));
        assert!(!input.has_pending_input());
    }

    fn secret(event: Option<InputEvent>) -> String {
        let Some(InputEvent::Paste(PasteOutcome::Secret { owner, text })) = event else {
            panic!("expected a secret paste, got {event:?}");
        };
        assert_eq!(owner, PasteOwner::AuthCode);
        text.expose().to_owned()
    }

    #[test]
    fn authorization_code_bytes_are_zeroed_in_fresh_input_as_they_are_consumed() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[200~code-123\x1b[201~");
        assert_eq!(
            input.next_event(context(0)),
            Some(action(Action::PasteStart))
        );
        assert_eq!(&input.fresh.storage()[..6], [0; 6]);
        assert_eq!(&input.fresh.storage()[6..14], b"code-123");
        input.begin_paste(PasteOwner::AuthCode, 64);
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.fresh.storage().len(), 20);
        assert!(input.fresh.storage().iter().all(|byte| *byte == 0));
        assert_eq!(secret(input.settle_delivery_epoch()), "code-123");
    }

    #[test]
    fn authorization_code_bytes_forwarded_by_the_theme_monitor_are_zeroed_in_staged_input() {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b[?997;1n");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.take_theme_query(0), Some(ThemeQuery::ResponseFence));
        input.push_bytes(b"\x1b[200~");
        assert_eq!(drain(&mut input, 1), vec![action(Action::PasteStart)]);
        input.begin_paste(PasteOwner::AuthCode, 64);
        for byte in b"code-123" {
            input.push_bytes(&[*byte]);
            assert!(drain(&mut input, 1).is_empty());
            assert_eq!(input.fresh.storage(), [0]);
            assert_eq!(input.staged.storage(), [0]);
        }
        input.push_bytes(b"\x1b[201~");
        assert!(drain(&mut input, 1).is_empty());
        assert!(input.staged.storage().iter().all(|byte| *byte == 0));
        assert_eq!(secret(input.settle_delivery_epoch()), "code-123");
    }

    #[test]
    fn dropping_input_mid_authorization_code_capture_does_not_reenter_drop() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[200~");
        assert_eq!(drain(&mut input, 0), vec![action(Action::PasteStart)]);
        input.begin_paste(PasteOwner::AuthCode, 64);
        input.push_bytes(b"half-entered-code");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.settle_delivery_epoch(), None);
        assert!(input.paste.active());
        drop(input);
    }

    #[test]
    fn theme_replies_are_filtered_out_of_input() {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        input.push_bytes(b"a\x1b[?997;2nb");
        assert_eq!(drain(&mut input, 0), vec![raw(b'a'), raw(b'b')]);
        assert_eq!(input.take_theme_query(0), Some(ThemeQuery::ResponseFence));
        input.push_bytes(b"\x1b[?1;2c\x1b]11;rgb:ffff/ffff/ffff\x07");
        assert!(drain(&mut input, 1).is_empty());
        assert_eq!(input.take_theme_query(2), Some(ThemeQuery::Background));
        input.push_bytes(b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?1;2c");
        assert!(drain(&mut input, 3).is_empty());
        let update = input.take_theme_update().unwrap();
        assert!(update.light);
    }

    #[test]
    fn a_failed_theme_query_settles_on_the_notified_scheme() {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b[?997;2n");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.take_theme_query(0), Some(ThemeQuery::ResponseFence));
        input.fail_theme_query();
        let update = input.take_theme_update().unwrap();
        assert!(update.light);
        assert_eq!(update.rgb, None);
        assert_eq!(input.take_theme_query(1), None);
    }

    #[test]
    fn a_user_escape_survives_the_theme_monitor_idle_timeout() {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b");
        assert!(drain(&mut input, 0).is_empty());
        input.poll_theme_monitor(75);
        assert!(drain(&mut input, 75).is_empty());
        assert_eq!(
            input.flush_escape(75 + INPUT_ESCAPE_TIMEOUT_MS),
            Some(action(Action::Escape))
        );
    }

    #[test]
    fn deferred_monitor_bytes_leave_nothing_pending_once_decoded() {
        let mut input = TerminalInput::new();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b[");
        assert!(drain(&mut input, 0).is_empty());
        input.poll_theme_monitor(75);
        input.push_bytes(b"A");
        assert_eq!(drain(&mut input, 75), vec![action(Action::CursorUp)]);
        assert!(!input.has_pending_input());
    }

    #[test]
    fn mouse_reports_decode_to_wheel_and_pointer_actions() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[<64;3;4M\x1b[<0;5;6M");
        assert_eq!(
            drain(&mut input, 0),
            vec![
                action(Action::MouseWheel(MouseWheel::Up)),
                action(Action::MousePointer(MousePointer {
                    kind: MousePointerKind::Press,
                    column: 5,
                    row: 6,
                    shift: false,
                    alt: false,
                    ctrl: false,
                })),
            ]
        );
    }
}
