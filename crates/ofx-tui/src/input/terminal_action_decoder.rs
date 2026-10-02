use super::escape_parser::{EscapeParser, MouseReportDiscardResult, control_byte_feature_action};
use super::input_action::{
    Action, DecodedTerminalAction, RawTerminalInput, ShortcutAction, TerminalDecodeContext,
    TerminalInputEvent, TerminalInputIngress,
};

const MOUSE_REPORT_ESCAPE_TIMEOUT_MS: i64 = 250;
const BELL: u8 = 0x07;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Decoder {
    pub(crate) parser: EscapeParser,
    pub(crate) started_ms: i64,
    pub(crate) cancel_pending: bool,
}

impl Decoder {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.parser.is_idle() && !self.parser.is_control_string()
    }

    pub(crate) fn in_control_string(&self) -> bool {
        self.parser.is_control_string()
    }

    pub(crate) fn feed(
        &mut self,
        byte: u8,
        context: TerminalDecodeContext,
    ) -> TerminalInputIngress {
        let mut ingress = TerminalInputIngress {
            interrupts_pending_text: byte < 0x80 && !context.paste_active,
            ..TerminalInputIngress::default()
        };

        if context.paste_active {
            return paste_byte_ingress(byte);
        }

        if byte == 0x1b && self.parser.is_idle() {
            self.begin_escape(context.now_ms, context.cancel_pending);
            return ingress;
        }

        if self.parser.is_mouse_report_discard() {
            match self.parser.consume_mouse_report_discard_byte(byte) {
                MouseReportDiscardResult::Pending => self.started_ms = context.now_ms,
                MouseReportDiscardResult::ReportEnd | MouseReportDiscardResult::Bound => {
                    self.finish_escape();
                }
                MouseReportDiscardResult::Restart => {
                    self.started_ms = context.now_ms;
                    self.cancel_pending = context.cancel_pending;
                }
            }
            return ingress;
        }

        if !self.parser.is_idle() {
            self.feed_escape_byte(byte, context, &mut ingress);
            return ingress;
        }

        match control_byte_feature_action(byte) {
            Some(resolved) => append_action(&mut ingress, resolved, false),
            None => append_raw(&mut ingress, byte),
        }
        ingress
    }

    fn feed_escape_byte(
        &mut self,
        byte: u8,
        context: TerminalDecodeContext,
        ingress: &mut TerminalInputIngress,
    ) {
        let prior_plain_bare_escape = self.parser.is_plain_bare_escape();
        let prior_x10_payload = self.parser.is_legacy_x10_payload();
        let prior_control_string = self.parser.is_control_string();
        let action = self.parser.consume(byte);

        if byte == 0x1b && !prior_x10_payload {
            self.started_ms = context.now_ms;
            self.cancel_pending = context.cancel_pending;
            return;
        }

        if !self.parser.is_idle() {
            self.started_ms = context.now_ms;
        }

        if prior_control_string && self.parser.is_idle() && byte < 0x20 && byte != BELL {
            let was_cancel_pending = self.take_cancel_pending();
            append_action(ingress, Action::Ignore, was_cancel_pending);
            ingress.replay_byte_after_routing = Some(byte);
            return;
        }

        if prior_plain_bare_escape
            && self.parser.is_idle()
            && matches!(action, None | Some(Action::Ignore))
            && should_replay_control_after_bare_escape(byte)
        {
            let was_cancel_pending = self.take_cancel_pending();
            append_action(ingress, Action::Escape, was_cancel_pending);
            ingress.replay_byte_after_routing = Some(byte);
            return;
        }

        if let Some(resolved) = action {
            let was_cancel_pending = self.take_cancel_pending();
            append_action(ingress, resolved, was_cancel_pending);
            return;
        }

        if self.parser.is_idle() {
            self.take_cancel_pending();
            append_action(ingress, Action::Ignore, false);
        }
    }

    pub(crate) fn flush(
        &mut self,
        now_ms: i64,
        timeout_ms: i64,
        paste_active: bool,
    ) -> TerminalInputIngress {
        let mut ingress = TerminalInputIngress::default();
        if self.parser.is_idle() || self.parser.is_control_string() {
            return ingress;
        }

        let effective_timeout_ms = if self.parser.is_mouse_report_payload() {
            timeout_ms.max(MOUSE_REPORT_ESCAPE_TIMEOUT_MS)
        } else {
            timeout_ms
        };
        if now_ms - self.started_ms < effective_timeout_ms {
            return ingress;
        }

        if paste_active
            || self.parser.is_control_sequence_discard()
            || self.parser.is_mouse_report_discard()
        {
            self.reset();
            return ingress;
        }

        if self.parser.begin_mouse_report_discard() {
            self.cancel_pending = false;
            self.started_ms = if self.parser.is_idle() { 0 } else { now_ms };
            return ingress;
        }

        if !self.parser.is_bare_escape() {
            self.reset();
            return ingress;
        }

        let was_cancel_pending = self.cancel_pending;
        self.reset();
        append_action(&mut ingress, Action::Escape, was_cancel_pending);
        ingress
    }

    fn begin_escape(&mut self, now_ms: i64, cancel_pending: bool) {
        self.parser.begin();
        self.started_ms = now_ms;
        self.cancel_pending = cancel_pending;
    }

    fn finish_escape(&mut self) {
        self.started_ms = 0;
        self.cancel_pending = false;
    }

    fn take_cancel_pending(&mut self) -> bool {
        std::mem::take(&mut self.cancel_pending)
    }
}

pub(crate) fn paste_byte_ingress(byte: u8) -> TerminalInputIngress {
    TerminalInputIngress {
        event: Some(TerminalInputEvent::PasteByte(byte)),
        ..TerminalInputIngress::default()
    }
}

fn append_raw(ingress: &mut TerminalInputIngress, byte: u8) {
    ingress.set_event(TerminalInputEvent::Raw(RawTerminalInput {
        byte,
        composer_shortcut: ShortcutAction::from_control_byte(byte),
    }));
}

fn append_action(ingress: &mut TerminalInputIngress, action: Action, cancel_pending: bool) {
    ingress.set_event(TerminalInputEvent::Action(DecodedTerminalAction {
        action,
        composer_shortcut: ShortcutAction::from_escape_action(action),
        cancel_pending,
    }));
}

fn should_replay_control_after_bare_escape(byte: u8) -> bool {
    ShortcutAction::from_control_byte(byte).is_some()
        || matches!(byte, 3 | 7 | 12 | 15 | 16 | 22 | 24)
}

#[cfg(test)]
mod tests {
    use super::super::escape_parser::Stage;
    use super::*;

    fn context(now_ms: i64, cancel_pending: bool) -> TerminalDecodeContext {
        TerminalDecodeContext {
            now_ms,
            paste_active: false,
            cancel_pending,
        }
    }

    fn decoded(ingress: TerminalInputIngress) -> DecodedTerminalAction {
        match ingress.event {
            Some(TerminalInputEvent::Action(decoded)) => decoded,
            other => panic!("expected a decoded action, got {other:?}"),
        }
    }

    #[test]
    fn plain_byte_carries_composer_fallback_without_consuming_product_routing() {
        let mut decoder = Decoder::default();
        let ingress = decoder.feed(11, context(1, false));
        assert_eq!(
            ingress.event,
            Some(TerminalInputEvent::Raw(RawTerminalInput {
                byte: 11,
                composer_shortcut: Some(ShortcutAction::DeleteToLineEnd),
            }))
        );
    }

    #[test]
    fn encoded_copy_and_cut_reports_carry_their_composer_intents() {
        let reports: [(&[u8], ShortcutAction); 4] = [
            (b"[99;9u", ShortcutAction::CopySelection),
            (b"[120;9u", ShortcutAction::CutSelection),
            (b"[27;9;99~", ShortcutAction::CopySelection),
            (b"[27;9;120~", ShortcutAction::CutSelection),
        ];
        for (report, intent) in reports {
            let mut decoder = Decoder::default();
            decoder.feed(0x1b, context(1, false));
            let (last, prefix) = report.split_last().unwrap();
            for byte in prefix {
                assert_eq!(decoder.feed(*byte, context(1, false)).event, None);
            }
            let action = decoded(decoder.feed(*last, context(1, false)));
            assert_eq!(
                action.action,
                Action::ComposerShortcut(intent),
                "{report:?}"
            );
            assert_eq!(action.composer_shortcut, Some(intent), "{report:?}");
        }
    }

    #[test]
    fn bare_escape_control_replay_preserves_event_order() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, true));
        let ingress = decoder.feed(3, context(2, true));
        let action = decoded(ingress);
        assert_eq!(action.action, Action::Escape);
        assert!(action.cancel_pending);
        assert_eq!(ingress.replay_byte_after_routing, Some(3));
    }

    #[test]
    fn escape_timeout_emits_one_semantic_action() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, true));
        let action = decoded(decoder.flush(31, 30, false));
        assert_eq!(action.action, Action::Escape);
        assert!(action.cancel_pending);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn active_paste_bypasses_decoding_and_preserves_pending_text_ownership() {
        let mut decoder = Decoder::default();
        let ingress = decoder.feed(
            0x1b,
            TerminalDecodeContext {
                now_ms: 1,
                paste_active: true,
                cancel_pending: true,
            },
        );
        assert_eq!(ingress.event, Some(TerminalInputEvent::PasteByte(0x1b)));
        assert!(!ingress.interrupts_pending_text);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn unknown_escape_resolves_to_ignore_instead_of_escape() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, true));
        let action = decoded(decoder.feed(b'x', context(2, false)));
        assert_eq!(action.action, Action::Ignore);
        assert!(action.cancel_pending);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn unknown_complete_csi_stays_pending_until_its_final_byte_and_resolves_to_ignore() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, true));
        for byte in *b"[>0" {
            let ingress = decoder.feed(byte, context(1, true));
            assert_eq!(ingress.event, None);
            assert!(decoder.has_pending());
        }
        let action = decoded(decoder.feed(b'q', context(1, true)));
        assert_eq!(action.action, Action::Ignore);
        assert!(action.cancel_pending);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn incomplete_csi_expires_without_producing_escape() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, true));
        decoder.feed(b'[', context(1, true));
        let ingress = decoder.flush(31, 30, false);
        assert_eq!(ingress.event, None);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn escape_waits_for_the_timeout_before_resolving() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, false));
        assert_eq!(decoder.flush(30, 30, false).event, None);
        assert!(decoder.has_pending());
        assert_eq!(decoded(decoder.flush(31, 30, false)).action, Action::Escape);
    }

    #[test]
    fn mouse_report_payload_extends_the_escape_timeout() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(0, false));
        for byte in *b"[<64" {
            decoder.feed(byte, context(0, false));
        }
        assert_eq!(decoder.flush(100, 30, false).event, None);
        assert_eq!(decoder.parser.stage, Stage::SgrMouseButton);
        assert_eq!(decoder.flush(250, 30, false).event, None);
        assert_eq!(decoder.parser.stage, Stage::DiscardedSgrMouse);
        for byte in *b";1;1" {
            assert_eq!(decoder.feed(byte, context(251, false)).event, None);
        }
        assert_eq!(decoder.feed(b'M', context(251, false)).event, None);
        assert!(!decoder.has_pending());
        assert_eq!(
            decoder.feed(b'a', context(252, false)).event,
            Some(TerminalInputEvent::Raw(RawTerminalInput {
                byte: b'a',
                composer_shortcut: None,
            }))
        );
    }

    #[test]
    fn meta_escape_does_not_replay_a_following_control_byte() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(1, false));
        decoder.feed(0x1b, context(1, false));
        let ingress = decoder.feed(3, context(1, false));
        assert_eq!(decoded(ingress).action, Action::Ignore);
        assert_eq!(ingress.replay_byte_after_routing, None);
    }

    #[test]
    fn reset_zeroes_terminal_decoder_state() {
        let mut decoder = Decoder::default();
        decoder.feed(0x1b, context(99, true));
        decoder.feed(b'[', context(99, true));
        decoder.feed(b'2', context(99, true));
        assert!(decoder.has_pending());
        decoder.reset();
        assert_eq!(decoder, Decoder::default());
        assert!(!decoder.has_pending());
    }
}
