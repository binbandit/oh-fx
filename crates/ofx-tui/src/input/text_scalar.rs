#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextOwner {
    Composer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextDropReason {
    InvalidUtf8,
    OwnerChanged,
    InterruptedByNonTextInput,
    PasteStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DroppedText {
    pub(crate) owner: TextOwner,
    pub(crate) bytes: usize,
    pub(crate) reason: TextDropReason,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct State {
    owner: Option<TextOwner>,
    bytes: [u8; 4],
    len: usize,
    expected_len: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) scalar: Option<char>,
    pub(crate) dropped: Option<DroppedText>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Transition {
    pub(crate) next: State,
    pub(crate) step: Step,
}

#[cfg(test)]
fn has_pending(state: State) -> bool {
    state.len > 0
}

pub(crate) fn reset(state: State, reason: TextDropReason) -> Option<DroppedText> {
    state.owner.map(|owner| DroppedText {
        owner,
        bytes: state.len,
        reason,
    })
}

pub(crate) fn advance(state: State, owner: TextOwner, byte: u8) -> Transition {
    if let Some(previous) = state.owner
        && previous != owner
    {
        return restart_after_drop(
            previous,
            state.len,
            TextDropReason::OwnerChanged,
            owner,
            byte,
        );
    }

    if state.len == 0 {
        return begin(owner, byte);
    }

    if byte & 0xc0 != 0x80 {
        return restart_after_drop(owner, state.len, TextDropReason::InvalidUtf8, owner, byte);
    }

    let mut next = state;
    next.bytes[next.len] = byte;
    next.len += 1;
    if next.len < next.expected_len {
        return Transition {
            next,
            step: Step::default(),
        };
    }

    let step = match decode(&next.bytes[..next.len]) {
        Some(scalar) => Step {
            scalar: Some(scalar),
            dropped: None,
        },
        None => Step {
            scalar: None,
            dropped: Some(DroppedText {
                owner,
                bytes: next.len,
                reason: TextDropReason::InvalidUtf8,
            }),
        },
    };
    Transition {
        next: State::default(),
        step,
    }
}

fn begin(owner: TextOwner, byte: u8) -> Transition {
    let Some(expected_len) = utf8_sequence_length(byte) else {
        return Transition {
            next: State::default(),
            step: Step {
                scalar: None,
                dropped: Some(DroppedText {
                    owner,
                    bytes: 1,
                    reason: TextDropReason::InvalidUtf8,
                }),
            },
        };
    };
    if expected_len == 1 {
        return Transition {
            next: State::default(),
            step: Step {
                scalar: Some(char::from(byte)),
                dropped: None,
            },
        };
    }
    Transition {
        next: State {
            owner: Some(owner),
            bytes: [byte, 0, 0, 0],
            len: 1,
            expected_len,
        },
        step: Step::default(),
    }
}

fn restart_after_drop(
    dropped_owner: TextOwner,
    dropped_bytes: usize,
    reason: TextDropReason,
    owner: TextOwner,
    byte: u8,
) -> Transition {
    let mut restarted = begin(owner, byte);
    let restarted_drop_bytes = restarted.step.dropped.map_or(0, |drop| drop.bytes);
    restarted.step.dropped = Some(DroppedText {
        owner: dropped_owner,
        bytes: dropped_bytes + restarted_drop_bytes,
        reason,
    });
    restarted
}

fn utf8_sequence_length(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

fn decode(bytes: &[u8]) -> Option<char> {
    std::str::from_utf8(bytes).ok()?.chars().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_scalar_admission_completes_utf8_atomically() {
        let mut transition = advance(State::default(), TextOwner::Composer, 0xe2);
        assert_eq!(transition.step, Step::default());
        transition = advance(transition.next, TextOwner::Composer, 0x82);
        assert_eq!(transition.step.scalar, None);
        transition = advance(transition.next, TextOwner::Composer, 0xac);
        assert_eq!(transition.step.scalar, Some('€'));
        assert!(!has_pending(transition.next));
    }

    #[test]
    fn text_scalar_admission_rejects_invalid_utf8_without_retaining_bytes() {
        let transition = advance(State::default(), TextOwner::Composer, 0xe2);
        let transition = advance(transition.next, TextOwner::Composer, b'(');
        assert_eq!(transition.step.scalar, Some('('));
        let dropped = transition.step.dropped.unwrap();
        assert_eq!(dropped.reason, TextDropReason::InvalidUtf8);
        assert_eq!(dropped.bytes, 1);
        assert!(!has_pending(transition.next));
    }

    #[test]
    fn text_scalar_reset_reports_pending_ownership_without_mutating_input() {
        let pending = advance(State::default(), TextOwner::Composer, 0xe2).next;
        let dropped = reset(pending, TextDropReason::PasteStarted).unwrap();
        assert_eq!(dropped.owner, TextOwner::Composer);
        assert_eq!(dropped.bytes, 1);
        assert_eq!(reset(State::default(), TextDropReason::PasteStarted), None);
    }

    #[test]
    fn overlong_sequences_are_dropped_after_completion() {
        let transition = advance(State::default(), TextOwner::Composer, 0xc0);
        let transition = advance(transition.next, TextOwner::Composer, 0x80);
        assert_eq!(transition.step.scalar, None);
        assert_eq!(transition.step.dropped.unwrap().bytes, 2);
    }
}
