mod escape_parser;
pub(crate) mod gesture_state;
mod ingress_queue;
mod input_action;
mod native_clear_probe;
mod paste_framing;
mod shortcuts;
mod terminal_action_decoder;
mod text_scalar;

use std::collections::VecDeque;

use crate::terminal::{
    ProbeFeed, ProbePoll, TaggedCursorProbe, ThemeMonitor, ThemeMonitorFeed, ThemeQuery,
    ThemeUpdate,
};

pub(crate) use input_action::{
    Action, DecodedTerminalAction, MoveIntent, MoveKind, RawTerminalInput, ShortcutAction,
};
pub(crate) use paste_framing::{
    COMPOSER_INPUT_LIMIT_BYTES, DECISION_INPUT_LIMIT_BYTES, PasteOutcome, PasteOwner,
};
pub(crate) use text_scalar::{DroppedText, TextDropReason, TextOwner};

use ingress_queue::IngressQueue;
use input_action::{TerminalDecodeContext, TerminalInputEvent};
use native_clear_probe::NativeClearProbe;
use paste_framing::PasteFraming;
use terminal_action_decoder::Decoder;

pub(crate) const INPUT_ESCAPE_TIMEOUT_MS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputContext {
    pub(crate) now_ms: i64,
    pub(crate) cancel_pending: bool,
    pub(crate) text_owner: TextOwner,
    pub(crate) native_clear_row: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputEvent {
    Action(DecodedTerminalAction),
    Raw(RawTerminalInput),
    Text(char),
    TextDropped(DroppedText),
    Paste(PasteOutcome),
    NativeClearProbe,
    NativeClearDetected,
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
    cursor_probe: TaggedCursorProbe,
    native_clear: NativeClearProbe,
    decoder: Decoder,
    paste: PasteFraming,
    text: text_scalar::State,
    fresh: IngressQueue,
    staged: IngressQueue,
    released: IngressQueue,
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

    pub(crate) fn settle_delivery_epoch(&mut self, now_ms: i64) -> Option<InputEvent> {
        if !self.paste.active() || self.has_unclassified_input() {
            return None;
        }
        let outcome = self.paste.settle_delivery_epoch()?;
        if !self.paste.active() {
            self.cursor_probe.resume_after_paste(now_ms);
        }
        Some(InputEvent::Paste(outcome))
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
        self.cursor_probe.suspend_for_paste();
    }

    pub(crate) fn has_pending_input(&self) -> bool {
        self.decoder.has_pending()
            || self.theme_monitor.has_pending_input()
            || !self.fresh.is_empty()
            || !self.staged.is_empty()
            || self.released_ready()
            || self.replay.is_some()
            || !self.events.is_empty()
    }

    pub(crate) fn awaiting_terminal_reply(&self) -> bool {
        self.theme_monitor.owns_input()
            || self.cursor_probe.intercepts_input()
            || self.decoder.holds_sequence()
    }

    pub(crate) fn start_native_clear_probe(&mut self) {
        self.native_clear.start();
    }

    pub(crate) fn native_clear_active(&self) -> bool {
        self.native_clear.active()
    }

    pub(crate) fn native_clear_busy(&self) -> bool {
        self.native_clear.busy()
    }

    pub(crate) fn native_clear_deadline_ms(&self) -> Option<i64> {
        self.cursor_probe.deadline_ms()
    }

    pub(crate) fn poll_native_clear_probe(&mut self, now_ms: i64) {
        match self.cursor_probe.poll(now_ms) {
            ProbePoll::Quiet => {}
            ProbePoll::TimedOut(unmatched) => {
                self.native_clear.disable(true);
                self.native_clear.settle();
                self.released.push(unmatched.as_slice());
            }
            ProbePoll::LateWindowExpired(unmatched) => {
                self.native_clear.finish_late_response();
                self.released.push(unmatched.as_slice());
            }
            ProbePoll::CandidateExpired(unmatched) => {
                for byte in unmatched.as_slice() {
                    self.paste.consume_byte(*byte);
                }
            }
        }
    }

    pub(crate) fn cancel_native_clear_probe(&mut self, turn_off: bool) {
        let unmatched = self.cursor_probe.cancel();
        if turn_off {
            self.native_clear.disable(false);
        }
        self.native_clear.settle();
        self.released.push(unmatched.as_slice());
    }

    pub(crate) fn start_theme_monitor(&mut self) {
        self.theme_monitor.start();
    }

    pub(crate) fn poll_theme_monitor(&mut self, now_ms: i64) {
        self.theme_monitor.poll(now_ms);
    }

    pub(crate) fn take_theme_query(&mut self, now_ms: i64) -> Option<ThemeQuery> {
        if self.theme_queries_held() {
            return None;
        }
        self.theme_monitor.take_query_request(now_ms)
    }

    pub(crate) fn theme_deadline_ms(&self, now_ms: i64) -> Option<i64> {
        self.theme_monitor
            .next_deadline_ms(now_ms, self.theme_queries_held())
    }

    fn theme_queries_held(&self) -> bool {
        self.paste.active() || self.native_clear.busy()
    }

    pub(crate) fn fail_theme_query(&mut self) {
        self.theme_monitor.fail_query();
    }

    pub(crate) fn take_theme_update(&mut self) -> Option<ThemeUpdate> {
        self.theme_monitor.take_settled_update()
    }

    fn has_unclassified_input(&self) -> bool {
        self.has_queued_input()
            || self.theme_monitor.has_pending_input()
            || self.cursor_probe.holds_candidate()
    }

    fn has_queued_input(&self) -> bool {
        !self.fresh.is_empty()
            || !self.staged.is_empty()
            || self.released_ready()
            || self.replay.is_some()
            || !self.events.is_empty()
            || self.theme_monitor.has_deferred_bytes()
    }

    fn released_ready(&self) -> bool {
        !self.native_clear.active() && !self.released.is_empty()
    }

    fn next_byte(&mut self) -> Option<(Entry, u8)> {
        if let Some(byte) = self.replay.take() {
            return Some((Entry::Decoder, byte));
        }
        if self.released_ready()
            && let Some(byte) = self.released.pop()
        {
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
                Owner::Paste => self.capture_paste_byte(byte, context.now_ms),
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
            self.capture_paste_byte(byte, context.now_ms);
        } else if self.cursor_probe.intercepts_input() {
            self.feed_cursor_probe(byte, context.now_ms);
        } else if let Some(row) = self.native_clear_row(byte, context) {
            self.begin_native_clear_probe(row, byte, context.now_ms);
        } else {
            self.decode(byte, context);
        }
    }

    fn native_clear_row(&self, byte: u8, context: InputContext) -> Option<u16> {
        let printable = byte >= 0x20 && byte != 0x7f;
        let ready = printable
            && self.native_clear.can_begin()
            && self.cursor_probe.can_begin()
            && !self.decoder.has_pending();
        context.native_clear_row.filter(|row| ready && *row > 0)
    }

    fn begin_native_clear_probe(&mut self, row: u16, byte: u8, now_ms: i64) {
        self.native_clear.begin(row);
        self.released.push(&[byte]);
        self.cursor_probe.begin(now_ms);
        self.events.push_back(InputEvent::NativeClearProbe);
    }

    fn capture_paste_byte(&mut self, byte: u8, now_ms: i64) {
        if !self.cursor_probe.discarding_late_reply() {
            self.paste.consume_byte(byte);
            return;
        }
        self.cursor_probe.note_input_activity(now_ms);
        match self.cursor_probe.feed(byte) {
            ProbeFeed::Pending => {}
            ProbeFeed::Position(_) | ProbeFeed::LateResponse => {
                self.native_clear.finish_late_response();
            }
            ProbeFeed::Forward(forwarded) => {
                for byte in forwarded.as_slice() {
                    self.paste.consume_byte(*byte);
                }
            }
        }
    }

    fn feed_cursor_probe(&mut self, byte: u8, now_ms: i64) {
        self.cursor_probe.note_input_activity(now_ms);
        match self.cursor_probe.feed(byte) {
            ProbeFeed::Pending => {}
            ProbeFeed::Position(position) => {
                if self.native_clear.settle() != Some(position.row) {
                    self.events.push_back(InputEvent::NativeClearDetected);
                }
            }
            ProbeFeed::LateResponse => self.native_clear.finish_late_response(),
            ProbeFeed::Forward(forwarded) => self.release_forwarded(forwarded.as_slice(), now_ms),
        }
    }

    fn release_forwarded(&mut self, forwarded: &[u8], now_ms: i64) {
        if self.native_clear.active() && self.native_clear.can_hold(forwarded.len()) {
            self.native_clear.hold(forwarded.len());
            self.released.push(forwarded);
            return;
        }
        if self.native_clear.active() {
            let unmatched = self.cursor_probe.expire(now_ms);
            self.native_clear.disable(true);
            self.native_clear.settle();
            self.released.push(forwarded);
            self.released.push(unmatched.as_slice());
            return;
        }
        self.released.push(forwarded);
    }

    fn decode(&mut self, byte: u8, context: InputContext) {
        let ingress = self.decoder.feed(
            byte,
            TerminalDecodeContext {
                now_ms: context.now_ms,
                paste_active: self.paste.active(),
                cancel_pending: context.cancel_pending,
                text_pending: text_scalar::has_pending(self.text),
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
            native_clear_row: None,
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

    fn probing(now_ms: i64) -> InputContext {
        InputContext {
            native_clear_row: Some(5),
            ..context(now_ms)
        }
    }

    fn drain_probing(input: &mut TerminalInput, now_ms: i64) -> Vec<InputEvent> {
        std::iter::from_fn(|| input.next_event(probing(now_ms))).collect()
    }

    fn probing_input() -> TerminalInput {
        let mut input = TerminalInput::new();
        input.start_native_clear_probe();
        input
    }

    #[test]
    fn a_printable_key_holds_input_until_the_cursor_reply_pair_arrives() {
        let mut input = probing_input();
        input.start_theme_monitor();
        input.push_bytes(b"ab\x1b[A");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        assert!(input.native_clear_active() && input.awaiting_terminal_reply());
        assert!(!input.has_pending_input());
        assert_eq!(input.native_clear_deadline_ms(), Some(100));
        input.push_bytes(b"\x1b[5;1R\x1b[5;2Rc");
        assert_eq!(
            drain_probing(&mut input, 10),
            [
                raw(b'a'),
                raw(b'b'),
                action(Action::CursorUp),
                InputEvent::NativeClearProbe
            ]
        );
        input.push_bytes(b"\x1b[5;1R\x1b[5;2R");
        assert_eq!(drain_probing(&mut input, 20), [raw(b'c')]);
        assert!(!input.native_clear_busy());
    }

    #[test]
    fn a_reply_on_another_row_reports_the_clear_before_the_held_keys() {
        let mut input = probing_input();
        input.push_bytes(b"x");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        input.push_bytes(b"\x1b[1;1R\x1b[1;2R");
        assert_eq!(
            drain_probing(&mut input, 5),
            [InputEvent::NativeClearDetected, raw(b'x')]
        );
    }

    #[test]
    fn only_a_printable_key_with_a_known_cursor_row_starts_a_probe() {
        let mut input = probing_input();
        input.push_bytes(b"\x01\x7f\x1b[Az");
        assert_eq!(
            drain(&mut input, 0),
            [raw(1), raw(0x7f), action(Action::CursorUp), raw(b'z')]
        );
        input.push_bytes(b"\x1b[");
        assert!(drain_probing(&mut input, 0).is_empty());
        input.push_bytes(b"A");
        assert_eq!(drain_probing(&mut input, 0), [action(Action::CursorUp)]);
        let mut unstarted = TerminalInput::new();
        unstarted.push_bytes(b"q");
        assert_eq!(drain_probing(&mut unstarted, 0), [raw(b'q')]);
    }

    #[test]
    fn a_timeout_releases_the_keys_turns_the_probe_off_and_discards_a_late_pair() {
        let mut input = probing_input();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b[?997;1n");
        assert!(drain(&mut input, 0).is_empty());
        input.push_bytes(b"a\x1b[5");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        assert_eq!(input.take_theme_query(0), None);
        input.poll_native_clear_probe(99);
        assert!(drain_probing(&mut input, 99).is_empty());
        input.poll_native_clear_probe(100);
        assert!(!input.native_clear_active() && input.native_clear_busy());
        assert_eq!(drain_probing(&mut input, 100), [raw(b'a')]);
        assert_eq!(input.take_theme_query(100), None);
        input.push_bytes(b";1R\x1b[5;1R\x1b[5;2Rb");
        assert_eq!(
            drain_probing(&mut input, 150),
            [action(Action::Ignore), raw(b'b')]
        );
        assert!(!input.native_clear_busy());
        assert_eq!(input.take_theme_query(150), Some(ThemeQuery::ResponseFence));
        input.push_bytes(b"c");
        assert_eq!(drain_probing(&mut input, 160), [raw(b'c')]);
    }

    #[test]
    fn the_late_window_hands_on_a_partial_reply_when_it_closes() {
        let mut input = probing_input();
        input.push_bytes(b"a");
        drain_probing(&mut input, 0);
        input.poll_native_clear_probe(100);
        assert_eq!(drain_probing(&mut input, 100), [raw(b'a')]);
        input.push_bytes(b"\x1b[");
        assert!(drain_probing(&mut input, 110).is_empty());
        assert_eq!(input.native_clear_deadline_ms(), Some(210));
        input.poll_native_clear_probe(210);
        assert!(!input.native_clear_busy());
        input.push_bytes(b"A");
        assert_eq!(drain_probing(&mut input, 210), [action(Action::CursorUp)]);
    }

    #[test]
    fn held_input_past_its_bound_is_released_in_arrival_order_and_ends_the_probe() {
        let mut input = probing_input();
        input.push_bytes(b"a");
        drain_probing(&mut input, 0);
        let mut burst = vec![b'b'; 4094];
        burst.extend_from_slice(b"cd\x1b[");
        input.push_bytes(&burst);
        let events = drain_probing(&mut input, 1);
        let keys: Vec<u8> = events
            .iter()
            .map(|event| match event {
                InputEvent::Raw(raw) => raw.byte,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let mut expected = vec![b'a'];
        expected.extend_from_slice(&[b'b'; 4094]);
        expected.extend_from_slice(b"cd");
        assert_eq!(keys, expected);
        assert!(!input.native_clear_active() && input.native_clear_busy());
        input.push_bytes(b"A");
        assert_eq!(drain_probing(&mut input, 2), [action(Action::CursorUp)]);
    }

    #[test]
    fn a_failed_query_write_releases_the_key_without_a_late_window() {
        let mut input = probing_input();
        input.push_bytes(b"a");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        input.cancel_native_clear_probe(true);
        assert!(!input.native_clear_busy());
        assert_eq!(drain_probing(&mut input, 0), [raw(b'a')]);
        input.push_bytes(b"b");
        assert_eq!(drain_probing(&mut input, 0), [raw(b'b')]);
    }

    #[test]
    fn a_query_the_terminal_had_no_room_for_releases_the_key_and_keeps_probing() {
        let mut input = probing_input();
        input.push_bytes(b"ab");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        input.cancel_native_clear_probe(false);
        assert!(!input.native_clear_busy());
        assert_eq!(drain_probing(&mut input, 0), [raw(b'a'), raw(b'b')]);
        input.push_bytes(b"c");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
    }

    #[test]
    fn a_paste_keeps_the_late_window_open_until_it_settles() {
        let mut input = probing_input();
        input.push_bytes(b"a\x1b[200~");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        input.poll_native_clear_probe(100);
        assert_eq!(
            drain_probing(&mut input, 100),
            [raw(b'a'), action(Action::PasteStart)]
        );
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        assert_eq!(input.native_clear_deadline_ms(), None);
        input.poll_native_clear_probe(1_000);
        assert!(input.native_clear_busy());
        input.push_bytes(b"text\x1b[201~");
        assert!(drain_probing(&mut input, 1_000).is_empty());
        assert!(matches!(
            input.settle_delivery_epoch(1_000),
            Some(InputEvent::Paste(PasteOutcome::Text { .. }))
        ));
        assert_eq!(input.native_clear_deadline_ms(), Some(1_100));
        input.push_bytes(b"\x1b[9;1R\x1b[9;2R");
        assert!(drain_probing(&mut input, 1_050).is_empty());
        assert!(!input.native_clear_busy());
    }

    #[test]
    fn a_late_reply_forwarded_by_the_theme_monitor_stays_out_of_the_paste() {
        let mut input = probing_input();
        input.start_theme_monitor();
        input.push_bytes(b"\x1b[?997;1n");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.take_theme_query(0), Some(ThemeQuery::ResponseFence));
        input.push_bytes(b"a\x1b[200~");
        assert_eq!(drain_probing(&mut input, 0), [InputEvent::NativeClearProbe]);
        input.poll_native_clear_probe(100);
        assert_eq!(
            drain_probing(&mut input, 100),
            [raw(b'a'), action(Action::PasteStart)]
        );
        input.begin_paste(PasteOwner::Composer, usize::MAX);
        assert_eq!(input.owner(), Owner::ThemeMonitor);
        input.push_bytes(b"left\x1b[5;1R\x1b[5;2Rright\x1b[201~");
        assert!(drain_probing(&mut input, 110).is_empty());
        assert_eq!(
            input.settle_delivery_epoch(110),
            Some(InputEvent::Paste(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "leftright".to_owned(),
            }))
        );
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
        assert_eq!(input.settle_delivery_epoch(0), None);
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(
            input.settle_delivery_epoch(0),
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
            input.settle_delivery_epoch(0),
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
        assert_eq!(input.settle_delivery_epoch(0), None);
        assert!(input.paste.active());
        input.push_bytes(b"def\x1b[201~");
        drain(&mut input, 1);
        assert_eq!(
            input.settle_delivery_epoch(0),
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
            input.settle_delivery_epoch(0),
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
            assert_eq!(input.settle_delivery_epoch(0), None, "{decoded}");
            input.push_bytes(rest);
            assert!(drain(&mut input, 2).is_empty(), "{decoded}");
            assert_eq!(
                input.settle_delivery_epoch(0),
                Some(trailing_input()),
                "{decoded}"
            );
            assert!(!input.has_pending_input(), "{decoded}");
        }
    }

    #[test]
    fn a_theme_reply_completing_after_the_end_marker_is_not_a_paste_suffix() {
        let mut input = paste_after_an_in_flight_theme_query(b"\x1b[?1;");
        assert_eq!(input.settle_delivery_epoch(0), None);
        input.push_bytes(b"2c");
        assert!(drain(&mut input, 2).is_empty());
        assert_eq!(
            input.settle_delivery_epoch(0),
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
        assert_eq!(input.settle_delivery_epoch(0), None);
        input.poll_theme_monitor(76);
        assert_eq!(input.settle_delivery_epoch(0), None);
        assert!(drain(&mut input, 76).is_empty());
        assert_eq!(input.settle_delivery_epoch(0), Some(trailing_input()));
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
        assert_eq!(secret(input.settle_delivery_epoch(0)), "code-123");
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
        assert_eq!(secret(input.settle_delivery_epoch(0)), "code-123");
    }

    #[test]
    fn dropping_input_mid_authorization_code_capture_does_not_reenter_drop() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[200~");
        assert_eq!(drain(&mut input, 0), vec![action(Action::PasteStart)]);
        input.begin_paste(PasteOwner::AuthCode, 64);
        input.push_bytes(b"half-entered-code");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.settle_delivery_epoch(0), None);
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

    const TERMINAL_STRINGS: [&[u8]; 21] = [
        b"\x9d11;rgb:1111/2222/3333\x9c",
        b"\x9d11;rgb:1/2/3\x07",
        b"\x901+r544e=787465726d\x9c",
        b"\x9b?997;1n",
        b"\x9b?62;22c",
        b"\x9fGi=1;OK 12\x9c",
        b"\x9eprivacy 12\x9c",
        b"\x98start 3\x9c",
        b"\x1b]11;rgb:2828/2c2c/3434\x1b\\",
        b"\x1b]11;rgba:2828/2c2c/3434/ffff\x1b\\",
        b"\x1b]11;rgb:1111/2222/3333\x07",
        b"\x1b]10;rgb:1/2/3\x07",
        b"\x1bP1+r544e=787465726d\x1b\\",
        b"\x1b_Gi=1;OK 12\x1b\\",
        b"\x1b^privacy 12\x1b\\",
        b"\x1bXstart of string 3\x1b\\",
        b"\x1b[?997;1n",
        b"\x1b[?62;22c",
        b"\x1b[?64;1;2;6;9;15;16;17;18;21;22;28c",
        b"\x1b[24;80R",
        b"\x1b[?2026;2$y",
    ];

    fn only_ignored(events: &[InputEvent]) -> bool {
        events.iter().all(|event| {
            matches!(
                event,
                InputEvent::Action(DecodedTerminalAction {
                    action: Action::Ignore,
                    ..
                })
            )
        })
    }

    #[test]
    fn terminal_strings_split_anywhere_never_decode_to_keys() {
        for monitored in [false, true] {
            for reply in TERMINAL_STRINGS {
                for split in (0..=reply.len()).filter(|split| *split != 1) {
                    let mut input = TerminalInput::new();
                    if monitored {
                        input.start_theme_monitor();
                    }
                    input.push_bytes(&reply[..split]);
                    let mut events = drain(&mut input, 0);
                    input.poll_theme_monitor(1_000);
                    events.extend(drain(&mut input, 1_000));
                    events.extend(input.flush_escape(1_000));
                    input.push_bytes(&reply[split..]);
                    events.extend(drain(&mut input, 2_000));
                    input.poll_theme_monitor(3_000);
                    events.extend(drain(&mut input, 3_000));
                    events.extend(input.flush_escape(3_000));
                    assert!(
                        only_ignored(&events),
                        "{monitored} {:?} | {:?}: {events:?}",
                        String::from_utf8_lossy(&reply[..split]),
                        String::from_utf8_lossy(&reply[split..])
                    );
                    input.push_bytes(b"1");
                    assert_eq!(drain(&mut input, 4_000), vec![raw(b'1')]);
                }
            }
        }
    }

    #[test]
    fn c1_valued_bytes_inside_utf8_text_never_start_or_end_a_sequence() {
        let mut input = TerminalInput::new();
        input.push_bytes("\u{dd}1\u{41b}2\u{271c}3".as_bytes());
        assert_eq!(
            drain(&mut input, 0),
            vec![
                InputEvent::Text('\u{dd}'),
                raw(b'1'),
                InputEvent::Text('\u{41b}'),
                raw(b'2'),
                InputEvent::Text('\u{271c}'),
                raw(b'3'),
            ]
        );
        let mut title = "\x1b]2;\u{271c}1\u{41b}2".as_bytes().to_vec();
        title.push(0x9c);
        input.push_bytes(&title);
        assert!(only_ignored(&drain(&mut input, 0)));
        input.push_bytes(b"x");
        assert_eq!(drain(&mut input, 0), vec![raw(b'x')]);
    }

    #[test]
    fn a_control_byte_ends_a_timed_out_control_sequence_and_still_counts() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b[?99");
        assert!(drain(&mut input, 0).is_empty());
        assert_eq!(input.flush_escape(10_000), None);
        assert!(input.awaiting_terminal_reply());
        input.push_bytes(b"\x03x");
        assert_eq!(
            drain(&mut input, 10_000),
            vec![action(Action::Ignore), raw(3), raw(b'x')]
        );
        assert!(!input.awaiting_terminal_reply());
    }

    #[test]
    fn a_control_byte_ends_an_unterminated_terminal_string_and_still_counts() {
        let mut input = TerminalInput::new();
        input.push_bytes(b"\x1b]draft");
        assert!(only_ignored(&drain(&mut input, 0)));
        assert_eq!(input.flush_escape(10_000), None);
        assert!(!input.has_pending_input());
        input.push_bytes(b"\x03x");
        assert_eq!(
            drain(&mut input, 10_000),
            vec![action(Action::Ignore), raw(3), raw(b'x')]
        );
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
