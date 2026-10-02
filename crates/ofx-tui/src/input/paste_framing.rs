use std::fmt;

use ofx_text::{is_model_safe_text, normalize_line_endings_in_place};
use zeroize::Zeroize;

const PASTE_END_MARKER: &[u8] = b"\x1b[201~";
const AUTH_CODE_RESERVED_BYTES: usize = 4096;

#[derive(PartialEq, Eq)]
pub(crate) struct SecretText(String);

impl SecretText {
    #[cfg(test)]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl Clone for SecretText {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText(..)")
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        wipe(&mut bytes);
    }
}

fn wipe(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.capacity(), 0);
    bytes.as_mut_slice().zeroize();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteOwner {
    Composer,
    DecisionPrompt,
    QuestionFreeform,
    AuthCode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasteOutcome {
    Text {
        owner: PasteOwner,
        text: String,
    },
    Secret {
        owner: PasteOwner,
        text: SecretText,
    },
    LimitExceeded {
        owner: PasteOwner,
        attempted_bytes: usize,
    },
    UnsupportedBytes {
        owner: PasteOwner,
    },
    TrailingInput {
        owner: PasteOwner,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Boundary {
    #[default]
    Capturing,
    EndCandidate,
    Unsafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settlement {
    None,
    Finish,
    Reject,
}

#[derive(Debug, Default)]
pub(crate) struct PasteFraming {
    owner: Option<PasteOwner>,
    boundary: Boundary,
    unsafe_suffix_bytes: usize,
    end_match_len: usize,
    end_candidate_buffer_len: usize,
    end_candidate_overflow_bytes: usize,
    overflow_bytes: usize,
    buffer_limit: usize,
    decision_bytes: usize,
    buffer: Vec<u8>,
}

impl PasteFraming {
    pub(crate) fn active(&self) -> bool {
        self.owner.is_some()
    }

    pub(crate) fn begin(&mut self, owner: PasteOwner, max_buffer_len: usize) {
        self.reset();
        self.owner = Some(owner);
        self.buffer_limit = max_buffer_len;
        if owner == PasteOwner::AuthCode {
            self.buffer
                .reserve(max_buffer_len.min(AUTH_CODE_RESERVED_BYTES));
        }
    }

    pub(crate) fn consume_byte(&mut self, byte: u8) {
        match self.boundary {
            Boundary::Capturing => {}
            Boundary::EndCandidate => {
                self.boundary = Boundary::Unsafe;
                self.unsafe_suffix_bytes = self.unsafe_suffix_bytes.saturating_add(1);
                return;
            }
            Boundary::Unsafe => {
                self.unsafe_suffix_bytes = self.unsafe_suffix_bytes.saturating_add(1);
                return;
            }
        }

        let Some(owner) = self.owner else {
            return;
        };
        if owner == PasteOwner::DecisionPrompt {
            self.decision_bytes = self.decision_bytes.saturating_add(1);
        } else {
            self.capture_byte(owner, byte);
        }

        if !self.advance_end_matcher(byte) {
            return;
        }
        if owner == PasteOwner::DecisionPrompt {
            self.decision_bytes = self.decision_bytes.saturating_sub(PASTE_END_MARKER.len());
        } else {
            self.buffer
                .truncate(self.buffer.len().min(self.end_candidate_buffer_len));
            self.overflow_bytes = self.end_candidate_overflow_bytes;
        }
        self.end_match_len = 0;
        self.end_candidate_buffer_len = 0;
        self.end_candidate_overflow_bytes = 0;
        self.boundary = Boundary::EndCandidate;
    }

    pub(crate) fn settle_delivery_epoch(&mut self) -> Option<PasteOutcome> {
        let owner = self.owner?;
        match self.settlement() {
            Settlement::None => None,
            Settlement::Finish => self.finish(owner),
            Settlement::Reject => {
                self.reset();
                Some(PasteOutcome::TrailingInput { owner })
            }
        }
    }

    pub(crate) fn reset(&mut self) {
        self.wipe_secret();
        self.owner = None;
        self.boundary = Boundary::Capturing;
        self.unsafe_suffix_bytes = 0;
        self.end_match_len = 0;
        self.end_candidate_buffer_len = 0;
        self.end_candidate_overflow_bytes = 0;
        self.overflow_bytes = 0;
        self.buffer_limit = usize::MAX;
        self.decision_bytes = 0;
        self.buffer.clear();
    }

    fn wipe_secret(&mut self) {
        if self.owner == Some(PasteOwner::AuthCode) {
            wipe(&mut self.buffer);
        }
    }

    fn capture_byte(&mut self, owner: PasteOwner, byte: u8) {
        if byte == 0x1b {
            self.end_candidate_buffer_len = self.buffer.len();
            self.end_candidate_overflow_bytes = self.overflow_bytes;
        }
        let captured = if owner == PasteOwner::QuestionFreeform && byte == b'\t' {
            b' '
        } else {
            byte
        };
        let printable = captured >= 32 && captured != 127;
        let accepted = match owner {
            PasteOwner::Composer => matches!(captured, b'\r' | b'\n' | b'\t') || printable,
            PasteOwner::QuestionFreeform => matches!(captured, b'\r' | b'\n') || printable,
            PasteOwner::AuthCode => printable,
            PasteOwner::DecisionPrompt => false,
        };
        if !accepted {
            return;
        }
        if self.buffer.len() < self.buffer_limit {
            self.buffer.push(captured);
        } else {
            self.overflow_bytes = self.overflow_bytes.saturating_add(1);
        }
    }

    fn settlement(&self) -> Settlement {
        match self.boundary {
            Boundary::Capturing => Settlement::None,
            Boundary::EndCandidate => Settlement::Finish,
            Boundary::Unsafe if self.confirmed_overflow_bytes() > 0 => Settlement::Finish,
            Boundary::Unsafe => Settlement::Reject,
        }
    }

    fn finish(&mut self, owner: PasteOwner) -> Option<PasteOutcome> {
        if self.overflow_bytes > 0 {
            let attempted_bytes = self.attempted_bytes();
            self.reset();
            return Some(PasteOutcome::LimitExceeded {
                owner,
                attempted_bytes,
            });
        }
        if owner == PasteOwner::DecisionPrompt {
            self.reset();
            return None;
        }
        if owner == PasteOwner::AuthCode {
            return Some(self.finish_secret(owner));
        }
        let mut bytes = std::mem::take(&mut self.buffer);
        self.reset();
        normalize_for_owner(owner, &mut bytes);
        match String::from_utf8(bytes) {
            Ok(text) if is_model_safe_text(text.as_bytes()) => {
                Some(PasteOutcome::Text { owner, text })
            }
            Ok(_) | Err(_) => Some(PasteOutcome::UnsupportedBytes { owner }),
        }
    }

    fn finish_secret(&mut self, owner: PasteOwner) -> PasteOutcome {
        let mut copy = Vec::with_capacity(self.buffer.len());
        copy.extend_from_slice(&self.buffer);
        self.reset();
        match String::from_utf8(copy) {
            Ok(text) => PasteOutcome::Secret {
                owner,
                text: SecretText(text),
            },
            Err(error) => {
                wipe(&mut error.into_bytes());
                PasteOutcome::UnsupportedBytes { owner }
            }
        }
    }

    fn attempted_bytes(&self) -> usize {
        self.buffer.len().saturating_add(self.overflow_bytes)
    }

    fn confirmed_overflow_bytes(&self) -> usize {
        if self.end_match_len > 0 {
            self.end_candidate_overflow_bytes
        } else {
            self.overflow_bytes
        }
    }

    fn advance_end_matcher(&mut self, byte: u8) -> bool {
        if byte == 0x1b {
            self.end_match_len = 1;
            return false;
        }
        if self.end_match_len == 0 {
            return false;
        }
        if byte != PASTE_END_MARKER[self.end_match_len] {
            self.end_match_len = 0;
            return false;
        }
        self.end_match_len += 1;
        self.end_match_len == PASTE_END_MARKER.len()
    }
}

impl Drop for PasteFraming {
    fn drop(&mut self) {
        self.wipe_secret();
    }
}

fn normalize_for_owner(owner: PasteOwner, bytes: &mut Vec<u8>) {
    match owner {
        PasteOwner::Composer | PasteOwner::QuestionFreeform => {
            normalize_line_endings_in_place(bytes);
        }
        PasteOwner::AuthCode | PasteOwner::DecisionPrompt => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framing(owner: PasteOwner, limit: usize) -> PasteFraming {
        let mut state = PasteFraming::default();
        state.begin(owner, limit);
        state
    }

    fn feed(state: &mut PasteFraming, bytes: &[u8]) {
        for byte in bytes {
            state.consume_byte(*byte);
        }
    }

    #[test]
    fn decision_paste_counts_content_bytes_and_exact_end_marker_only() {
        let mut state = framing(PasteOwner::DecisionPrompt, usize::MAX);
        feed(&mut state, b"abc");
        feed(&mut state, &PASTE_END_MARKER[..PASTE_END_MARKER.len() - 1]);
        feed(&mut state, &PASTE_END_MARKER[PASTE_END_MARKER.len() - 1..]);
        assert_eq!(state.decision_bytes, 3);
        assert_eq!(state.owner, Some(PasteOwner::DecisionPrompt));
        assert_eq!(state.boundary, Boundary::EndCandidate);
        assert_eq!(state.settlement(), Settlement::Finish);
    }

    #[test]
    fn decision_paste_treats_incomplete_and_parameterized_end_candidates_as_content() {
        let mut state = framing(PasteOwner::DecisionPrompt, usize::MAX);
        let payload = b"\x1b[20\x1b[201;1~x";
        feed(&mut state, payload);
        feed(&mut state, PASTE_END_MARKER);
        assert_eq!(state.decision_bytes, payload.len());
        assert_eq!(state.settlement(), Settlement::Finish);
    }

    #[test]
    fn composer_paste_preserves_safe_bytes_and_tracks_bounded_overflow() {
        let mut state = framing(PasteOwner::Composer, 3);
        feed(&mut state, b"abcd\x01");
        feed(&mut state, PASTE_END_MARKER);
        assert_eq!(state.buffer, b"abc");
        assert_eq!(state.overflow_bytes, 1);
        assert_eq!(state.attempted_bytes(), 4);
    }

    #[test]
    fn composer_overflow_excludes_a_possible_exact_end_marker() {
        let mut exact = framing(PasteOwner::Composer, 1);
        exact.consume_byte(b'x');
        for byte in &PASTE_END_MARKER[..PASTE_END_MARKER.len() - 1] {
            exact.consume_byte(*byte);
            assert_eq!(exact.confirmed_overflow_bytes(), 0);
        }
        exact.consume_byte(PASTE_END_MARKER[PASTE_END_MARKER.len() - 1]);
        assert_eq!(exact.confirmed_overflow_bytes(), 0);

        let mut mismatch = framing(PasteOwner::Composer, 1);
        mismatch.consume_byte(b'x');
        feed(&mut mismatch, b"\x1b[20x");
        assert!(mismatch.confirmed_overflow_bytes() > 0);
    }

    #[test]
    fn composer_overflow_rejection_takes_precedence_over_an_unsafe_suffix() {
        let mut state = framing(PasteOwner::Composer, 1);
        feed(&mut state, b"xy");
        feed(&mut state, PASTE_END_MARKER);
        state.consume_byte(b'z');
        assert_eq!(state.boundary, Boundary::Unsafe);
        assert!(state.confirmed_overflow_bytes() > 0);
        assert_eq!(state.settlement(), Settlement::Finish);
        assert_eq!(
            state.settle_delivery_epoch(),
            Some(PasteOutcome::LimitExceeded {
                owner: PasteOwner::Composer,
                attempted_bytes: 2,
            })
        );
        assert!(!state.active());
    }

    #[test]
    fn question_paste_keeps_its_owner_specific_safe_bytes() {
        let mut question = framing(PasteOwner::QuestionFreeform, usize::MAX);
        feed(&mut question, b"one\r\ttwo\x03");
        feed(&mut question, PASTE_END_MARKER);
        assert_eq!(question.buffer, b"one\r two");
    }

    #[test]
    fn capturing_remains_active_when_a_delivery_epoch_ends_before_the_marker() {
        let mut state = framing(PasteOwner::Composer, usize::MAX);
        state.consume_byte(b'x');
        assert!(state.active());
        assert_eq!(state.boundary, Boundary::Capturing);
        assert_eq!(state.settle_delivery_epoch(), None);
        assert!(state.active());
    }

    #[test]
    fn end_candidate_stays_paste_owned_until_delivery_epoch_settlement() {
        let mut state = framing(PasteOwner::Composer, usize::MAX);
        feed(&mut state, b"safe");
        feed(&mut state, PASTE_END_MARKER);
        assert!(state.active());
        assert_eq!(state.buffer, b"safe");
        assert_eq!(state.boundary, Boundary::EndCandidate);
        assert_eq!(
            state.settle_delivery_epoch(),
            Some(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "safe".to_owned(),
            })
        );
        assert!(!state.active());
    }

    #[test]
    fn any_same_epoch_suffix_makes_the_candidate_sticky_unsafe() {
        let mut state = framing(PasteOwner::Composer, usize::MAX);
        feed(&mut state, b"safe");
        feed(&mut state, PASTE_END_MARKER);
        state.consume_byte(b'\r');
        feed(&mut state, PASTE_END_MARKER);
        assert!(state.active());
        assert_eq!(state.buffer, b"safe");
        assert_eq!(state.boundary, Boundary::Unsafe);
        assert_eq!(state.unsafe_suffix_bytes, 1 + PASTE_END_MARKER.len());
        assert_eq!(state.settlement(), Settlement::Reject);
        assert_eq!(
            state.settle_delivery_epoch(),
            Some(PasteOutcome::TrailingInput {
                owner: PasteOwner::Composer
            })
        );
    }

    #[test]
    fn finished_composer_paste_normalizes_line_endings() {
        let mut state = framing(PasteOwner::Composer, usize::MAX);
        feed(&mut state, b"one\r\ntwo\rthree\n");
        feed(&mut state, PASTE_END_MARKER);
        assert_eq!(
            state.settle_delivery_epoch(),
            Some(PasteOutcome::Text {
                owner: PasteOwner::Composer,
                text: "one\ntwo\nthree\n".to_owned(),
            })
        );
    }

    #[test]
    fn finished_paste_rejects_invalid_utf8() {
        let mut state = framing(PasteOwner::Composer, usize::MAX);
        feed(&mut state, b"ok\xff");
        feed(&mut state, PASTE_END_MARKER);
        assert_eq!(
            state.settle_delivery_epoch(),
            Some(PasteOutcome::UnsupportedBytes {
                owner: PasteOwner::Composer
            })
        );
    }

    #[test]
    fn authorization_code_paste_zeroes_retained_capacity_after_handling_and_reset() {
        let mut framing = PasteFraming::default();
        framing.begin(PasteOwner::AuthCode, 64);
        for byte in b"temporary-code\x1b[201~" {
            framing.consume_byte(*byte);
        }
        let outcome = framing.settle_delivery_epoch().unwrap();
        let PasteOutcome::Secret { text, .. } = &outcome else {
            panic!("expected a secret outcome, got {outcome:?}");
        };
        assert_eq!(text.expose(), "temporary-code");
        assert_eq!(
            format!("{outcome:?}"),
            "Secret { owner: AuthCode, text: SecretText(..) }"
        );
        assert!(framing.buffer.is_empty());
        assert!(framing.buffer.capacity() >= 64);

        framing.begin(PasteOwner::AuthCode, 64);
        for byte in b"second-code" {
            framing.consume_byte(*byte);
        }
        framing.reset();
        assert!(framing.buffer.is_empty());

        let mut retained = Vec::with_capacity(32);
        retained.extend_from_slice(b"secret");
        retained.truncate(2);
        wipe(&mut retained);
        assert_eq!(retained.len(), retained.capacity());
        assert!(retained.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn an_unfinished_authorization_code_capture_is_wiped_in_place_before_it_is_freed() {
        let mut state = framing(PasteOwner::AuthCode, 64);
        feed(&mut state, b"half-entered-code");
        state.wipe_secret();
        assert!(state.buffer.capacity() >= 64);
        assert_eq!(state.buffer.len(), state.buffer.capacity());
        assert!(state.buffer.iter().all(|byte| *byte == 0));
        assert!(state.active());

        let mut composer = framing(PasteOwner::Composer, 64);
        feed(&mut composer, b"draft");
        composer.wipe_secret();
        assert_eq!(composer.buffer, b"draft");
    }

    #[test]
    fn authorization_code_paste_rejects_invalid_utf8_without_text() {
        let mut framing = PasteFraming::default();
        framing.begin(PasteOwner::AuthCode, 64);
        for byte in b"\xff\xfe" {
            framing.capture_byte(PasteOwner::AuthCode, *byte);
        }
        framing.boundary = Boundary::EndCandidate;
        assert_eq!(
            framing.settle_delivery_epoch(),
            Some(PasteOutcome::UnsupportedBytes {
                owner: PasteOwner::AuthCode
            })
        );
        assert!(framing.buffer.is_empty());
    }

    #[test]
    fn finished_decision_prompt_paste_is_discarded() {
        let mut state = framing(PasteOwner::DecisionPrompt, usize::MAX);
        feed(&mut state, b"1");
        feed(&mut state, PASTE_END_MARKER);
        assert_eq!(state.settle_delivery_epoch(), None);
        assert!(!state.active());
    }
}
