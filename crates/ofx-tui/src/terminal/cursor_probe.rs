use std::ops::Range;

use super::forwarded_bytes::ForwardedBytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPosition {
    pub(crate) row: u16,
    pub(crate) col: u16,
}

fn count_digits(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

pub(crate) fn find_position_response(bytes: &[u8]) -> Option<CursorPosition> {
    find_position_span(bytes).map(|(_, position)| position)
}

pub(crate) fn find_position_span(bytes: &[u8]) -> Option<(Range<usize>, CursorPosition)> {
    (0..bytes.len()).find_map(|start| {
        position_response_at(bytes, start).map(|(end, position)| (start..end, position))
    })
}

fn position_response_at(bytes: &[u8], start: usize) -> Option<(usize, CursorPosition)> {
    let rest = bytes.get(start..)?.strip_prefix(b"\x1b[")?;
    let (row, rest) = split_digits(rest)?;
    let rest = rest.strip_prefix(b";")?;
    let (col, rest) = split_digits(rest)?;
    let rest = rest.strip_prefix(b"R")?;
    let row = parse_coordinate(row)?;
    let col = parse_coordinate(col)?;
    Some((bytes.len() - rest.len(), CursorPosition { row, col }))
}

fn split_digits(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let digits = count_digits(bytes);
    if digits == 0 {
        return None;
    }
    Some(bytes.split_at(digits))
}

fn parse_coordinate(digits: &[u8]) -> Option<u16> {
    let value: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (value != 0).then_some(value)
}

pub(crate) const TAGGED_CURSOR_QUERY: &str =
    "\x1b[?2026h\x1b7\x1b[1G\x1b[6n\x1b[2G\x1b[6n\x1b8\x1b[?2026l";
pub(crate) const CONFIRMATION_TAG_COLUMN: u16 = 2;
const FIRST_TAG_COLUMN: u16 = 1;
const RESPONSE_IDLE_TIMEOUT_MS: i64 = 100;
const MAX_COORDINATE_DIGITS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeFeed {
    Pending,
    Forward(ForwardedBytes),
    Position(CursorPosition),
    LateResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbePoll {
    Quiet,
    TimedOut(ForwardedBytes),
    LateWindowExpired(ForwardedBytes),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Idle,
    Waiting,
    DiscardingLate,
    DiscardingLateUntilPasteEnds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Candidate {
    Prefix,
    Complete(CursorPosition),
    Invalid,
}

#[derive(Debug, Default)]
pub(crate) struct TaggedCursorProbe {
    mode: Mode,
    deadline_ms: i64,
    candidate: ForwardedBytes,
    first_reply: ForwardedBytes,
    first_position: Option<CursorPosition>,
}

impl TaggedCursorProbe {
    pub(crate) fn begin(&mut self, now_ms: i64) {
        debug_assert!(self.can_begin());
        self.mode = Mode::Waiting;
        self.deadline_ms = now_ms.saturating_add(RESPONSE_IDLE_TIMEOUT_MS);
        self.take_unmatched();
    }

    pub(crate) fn cancel(&mut self) -> ForwardedBytes {
        self.mode = Mode::Idle;
        self.deadline_ms = 0;
        self.take_unmatched()
    }

    pub(crate) fn can_begin(&self) -> bool {
        self.mode == Mode::Idle
    }

    pub(crate) fn intercepts_input(&self) -> bool {
        self.mode != Mode::Idle
    }

    pub(crate) fn discarding_late_reply(&self) -> bool {
        matches!(
            self.mode,
            Mode::DiscardingLate | Mode::DiscardingLateUntilPasteEnds
        )
    }

    pub(crate) fn deadline_ms(&self) -> Option<i64> {
        matches!(self.mode, Mode::Waiting | Mode::DiscardingLate).then_some(self.deadline_ms)
    }

    pub(crate) fn note_input_activity(&mut self, now_ms: i64) {
        if matches!(self.mode, Mode::Waiting | Mode::DiscardingLate) {
            self.deadline_ms = now_ms.saturating_add(RESPONSE_IDLE_TIMEOUT_MS);
        }
    }

    pub(crate) fn suspend_for_paste(&mut self) {
        if self.mode == Mode::DiscardingLate {
            self.mode = Mode::DiscardingLateUntilPasteEnds;
            self.deadline_ms = 0;
        }
    }

    pub(crate) fn resume_after_paste(&mut self, now_ms: i64) {
        if self.mode == Mode::DiscardingLateUntilPasteEnds {
            self.mode = Mode::DiscardingLate;
            self.deadline_ms = now_ms.saturating_add(RESPONSE_IDLE_TIMEOUT_MS);
        }
    }

    pub(crate) fn poll(&mut self, now_ms: i64) -> ProbePoll {
        match self.mode {
            Mode::Idle | Mode::DiscardingLateUntilPasteEnds => ProbePoll::Quiet,
            _ if now_ms < self.deadline_ms => ProbePoll::Quiet,
            Mode::Waiting => ProbePoll::TimedOut(self.expire(now_ms)),
            Mode::DiscardingLate => {
                self.mode = Mode::Idle;
                self.deadline_ms = 0;
                ProbePoll::LateWindowExpired(self.take_unmatched())
            }
        }
    }

    pub(crate) fn expire(&mut self, now_ms: i64) -> ForwardedBytes {
        debug_assert_eq!(self.mode, Mode::Waiting);
        self.mode = Mode::DiscardingLate;
        self.deadline_ms = now_ms.saturating_add(RESPONSE_IDLE_TIMEOUT_MS);
        self.take_unmatched()
    }

    pub(crate) fn feed(&mut self, byte: u8) -> ProbeFeed {
        if !self.intercepts_input() {
            return ProbeFeed::Forward(ForwardedBytes::single(byte));
        }
        self.candidate.push(byte);
        match classify(self.candidate.as_slice()) {
            Candidate::Prefix => ProbeFeed::Pending,
            Candidate::Complete(position) => self.handle_position(position),
            Candidate::Invalid => self.forward_invalid_candidate(),
        }
    }

    fn handle_position(&mut self, position: CursorPosition) -> ProbeFeed {
        let Some(first) = self.first_position else {
            if position.col != FIRST_TAG_COLUMN {
                return self.forward_invalid_candidate();
            }
            self.store_first_reply(position);
            return ProbeFeed::Pending;
        };
        if position.row == first.row && position.col == CONFIRMATION_TAG_COLUMN {
            return self.complete(first);
        }
        let mut forwarded = self.first_reply;
        self.first_reply.clear();
        self.first_position = None;
        if position.col == FIRST_TAG_COLUMN {
            self.store_first_reply(position);
        } else {
            forwarded.extend_from_slice(self.candidate.as_slice());
            self.candidate.clear();
        }
        ProbeFeed::Forward(forwarded)
    }

    fn store_first_reply(&mut self, position: CursorPosition) {
        self.first_reply = self.candidate;
        self.first_position = Some(position);
        self.candidate.clear();
    }

    fn complete(&mut self, position: CursorPosition) -> ProbeFeed {
        let waiting = self.mode == Mode::Waiting;
        self.mode = Mode::Idle;
        self.deadline_ms = 0;
        self.take_unmatched();
        if waiting {
            ProbeFeed::Position(position)
        } else {
            ProbeFeed::LateResponse
        }
    }

    fn forward_invalid_candidate(&mut self) -> ProbeFeed {
        let candidate = self.candidate;
        let bytes = candidate.as_slice();
        let suffix = (1..bytes.len())
            .find(|start| classify(&bytes[*start..]) == Candidate::Prefix)
            .unwrap_or(bytes.len());
        let mut forwarded = self.first_reply;
        forwarded.extend_from_slice(&bytes[..suffix]);
        self.first_reply.clear();
        self.first_position = None;
        self.candidate = ForwardedBytes::from_slice(&bytes[suffix..]);
        ProbeFeed::Forward(forwarded)
    }

    fn take_unmatched(&mut self) -> ForwardedBytes {
        let mut unmatched = self.first_reply;
        unmatched.extend_from_slice(self.candidate.as_slice());
        self.first_reply.clear();
        self.first_position = None;
        self.candidate.clear();
        unmatched
    }
}

fn classify(bytes: &[u8]) -> Candidate {
    let rest = match bytes {
        [] | [0x1b] => return Candidate::Prefix,
        [0x1b, b'[', rest @ ..] | [0x9b, rest @ ..] => rest,
        _ => return Candidate::Invalid,
    };
    let (row, rest) = rest.split_at(count_digits(rest));
    if row.len() > MAX_COORDINATE_DIGITS {
        return Candidate::Invalid;
    }
    let Some((&separator, rest)) = rest.split_first() else {
        return Candidate::Prefix;
    };
    if row.is_empty() || separator != b';' {
        return Candidate::Invalid;
    }
    let (col, rest) = rest.split_at(count_digits(rest));
    if col.len() > MAX_COORDINATE_DIGITS {
        return Candidate::Invalid;
    }
    match rest {
        [] => Candidate::Prefix,
        [b'R'] if !col.is_empty() => match (parse_coordinate(row), parse_coordinate(col)) {
            (Some(row), Some(col)) => Candidate::Complete(CursorPosition { row, col }),
            _ => Candidate::Invalid,
        },
        _ => Candidate::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_cursor_response_parser_finds_a_response_with_leading_and_trailing_bytes() {
        assert_eq!(
            find_position_response(b"noise\x1b[42;7Rtail"),
            Some(CursorPosition { row: 42, col: 7 })
        );
    }

    #[test]
    fn position_spans_cover_only_the_reply_bytes() {
        assert_eq!(
            find_position_span(b"ab\x1b[7;3Rc"),
            Some((2..8, CursorPosition { row: 7, col: 3 }))
        );
        assert_eq!(find_position_span(b"\x1b[7;3"), None);
    }

    #[test]
    fn one_shot_cursor_response_parser_rejects_invalid_rows_and_columns() {
        let invalid: [&[u8]; 9] = [
            b"",
            b"\x1b[0;1R",
            b"\x1b[1;0R",
            b"\x1b[;1R",
            b"\x1b[1;R",
            b"\x1b[1;1",
            b"\x1b[999999;1R",
            b"\x1b[1;999999R",
            b"\x9b1;1R",
        ];
        for sample in invalid {
            assert_eq!(find_position_response(sample), None);
        }
    }

    fn waiting() -> TaggedCursorProbe {
        let mut probe = TaggedCursorProbe::default();
        probe.begin(0);
        probe
    }

    fn feed_all(probe: &mut TaggedCursorProbe, bytes: &[u8]) -> (Vec<u8>, Vec<ProbeFeed>) {
        let mut forwarded = Vec::new();
        let mut results = Vec::new();
        for byte in bytes {
            match probe.feed(*byte) {
                ProbeFeed::Pending => {}
                ProbeFeed::Forward(bytes) => forwarded.extend_from_slice(bytes.as_slice()),
                result => results.push(result),
            }
        }
        (forwarded, results)
    }

    fn position(row: u16) -> ProbeFeed {
        ProbeFeed::Position(CursorPosition { row, col: 1 })
    }

    #[test]
    fn the_tagged_query_synchronizes_its_transient_cursor_moves() {
        assert_eq!(
            TAGGED_CURSOR_QUERY,
            "\x1b[?2026h\x1b7\x1b[1G\x1b[6n\x1b[2G\x1b[6n\x1b8\x1b[?2026l"
        );
    }

    #[test]
    fn a_reply_pair_reports_the_row_and_leaves_backlog_before_it_as_input() {
        for backlog in [255, 256, 257, 100_000] {
            let mut probe = waiting();
            let mut bytes = vec![b'x'; backlog];
            bytes.extend_from_slice(b"\x1b[12;1R\x1b[12;2R");
            let (forwarded, results) = feed_all(&mut probe, &bytes);
            assert_eq!(forwarded, vec![b'x'; backlog]);
            assert_eq!(results, [position(12)]);
            assert!(probe.can_begin());
        }
    }

    #[test]
    fn a_reply_split_across_reads_is_still_consumed() {
        let mut probe = waiting();
        assert_eq!(feed_all(&mut probe, b"\x1b[7;"), (Vec::new(), Vec::new()));
        assert_eq!(feed_all(&mut probe, b"1R"), (Vec::new(), Vec::new()));
        assert_eq!(
            feed_all(&mut probe, b"\x1b[7;2R"),
            (Vec::new(), vec![position(7)])
        );
        assert!(!probe.intercepts_input());
    }

    #[test]
    fn modified_f3_before_the_reply_stays_input() {
        let mut probe = waiting();
        let (forwarded, results) = feed_all(&mut probe, b"\x1b[1;2R\x1b[12;1R\x1b[12;2R");
        assert_eq!(forwarded, b"\x1b[1;2R");
        assert_eq!(results, [position(12)]);
    }

    #[test]
    fn every_legacy_f3_modifier_stays_input() {
        for modifier in 2..=64 {
            let mut probe = waiting();
            let sequence = format!("\x1b[1;{modifier}R");
            let (forwarded, results) = feed_all(&mut probe, sequence.as_bytes());
            assert_eq!(forwarded, sequence.as_bytes());
            assert!(results.is_empty());
        }
    }

    #[test]
    fn the_modified_f3_three_and_four_pair_stays_input() {
        let mut probe = waiting();
        let (forwarded, results) = feed_all(&mut probe, b"\x1b[1;3R\x1b[1;4R\x1b[12;1R\x1b[12;2R");
        assert_eq!(forwarded, b"\x1b[1;3R\x1b[1;4R");
        assert_eq!(results, [position(12)]);
    }

    #[test]
    fn report_shaped_input_after_the_reply_stays_input() {
        let mut probe = waiting();
        let (forwarded, results) = feed_all(&mut probe, b"\x1b[12;1R\x1b[12;2R\x1b[1;2R");
        assert_eq!(forwarded, b"\x1b[1;2R");
        assert_eq!(results, [position(12)]);
    }

    #[test]
    fn untagged_reports_and_keys_stay_input_in_order() {
        let mut probe = waiting();
        let (forwarded, results) = feed_all(&mut probe, b"\x1b[1;2Rx\x1b[7;1R\x1b[7;2R");
        assert_eq!(forwarded, b"\x1b[1;2Rx");
        assert_eq!(results, [position(7)]);
    }

    #[test]
    fn consecutive_probes_accept_eight_bit_replies() {
        let mut probe = waiting();
        assert_eq!(feed_all(&mut probe, b"\x1b[2;1R\x1b[2;2R").1, [position(2)]);
        probe.begin(0);
        assert_eq!(feed_all(&mut probe, b"\x9b4;1R\x9b4;2R").1, [position(4)]);
    }

    #[test]
    fn a_timeout_hands_back_a_partial_reply_and_discards_the_late_pair() {
        let mut probe = waiting();
        assert_eq!(feed_all(&mut probe, b"\x1b[12"), (Vec::new(), Vec::new()));
        assert_eq!(probe.poll(99), ProbePoll::Quiet);
        assert_eq!(
            probe.poll(100),
            ProbePoll::TimedOut(ForwardedBytes::from_slice(b"\x1b[12"))
        );
        assert!(probe.intercepts_input() && !probe.can_begin());
        assert_eq!(
            feed_all(&mut probe, b"\x1b[8;1R\x1b[8;2R"),
            (Vec::new(), vec![ProbeFeed::LateResponse])
        );
        assert!(probe.can_begin());
    }

    #[test]
    fn the_late_window_closes_after_its_own_timeout() {
        let mut probe = waiting();
        probe.poll(100);
        assert_eq!(probe.deadline_ms(), Some(200));
        assert_eq!(feed_all(&mut probe, b"\x1b[8").0, b"");
        assert_eq!(
            probe.poll(200),
            ProbePoll::LateWindowExpired(ForwardedBytes::from_slice(b"\x1b[8"))
        );
        assert!(probe.can_begin());
        assert_eq!(probe.deadline_ms(), None);
    }

    #[test]
    fn the_timeout_measures_input_silence_rather_than_total_time() {
        let mut probe = waiting();
        probe.note_input_activity(90);
        assert_eq!(probe.poll(189), ProbePoll::Quiet);
        assert!(matches!(probe.poll(190), ProbePoll::TimedOut(_)));
    }

    #[test]
    fn a_paste_holds_the_late_window_open_until_it_ends() {
        let mut probe = waiting();
        probe.poll(100);
        probe.suspend_for_paste();
        assert!(probe.intercepts_input());
        assert_eq!(probe.deadline_ms(), None);
        assert_eq!(probe.poll(1_000), ProbePoll::Quiet);
        probe.resume_after_paste(1_000);
        assert_eq!(probe.deadline_ms(), Some(1_100));
        assert_eq!(
            feed_all(&mut probe, b"\x1b[12;1R\x1b[12;2R"),
            (Vec::new(), vec![ProbeFeed::LateResponse])
        );
        assert!(probe.can_begin());
    }

    #[test]
    fn a_paste_does_not_pause_a_probe_still_waiting_for_its_reply() {
        let mut probe = waiting();
        probe.suspend_for_paste();
        assert_eq!(probe.deadline_ms(), Some(100));
        probe.resume_after_paste(50);
        assert_eq!(probe.deadline_ms(), Some(100));
    }

    #[test]
    fn a_cancelled_probe_hands_back_what_it_held() {
        let mut probe = waiting();
        feed_all(&mut probe, b"\x1b[3;1R\x1b[");
        assert_eq!(
            probe.cancel(),
            ForwardedBytes::from_slice(b"\x1b[3;1R\x1b[")
        );
        assert!(probe.can_begin());
        assert_eq!(
            probe.feed(b'a'),
            ProbeFeed::Forward(ForwardedBytes::single(b'a'))
        );
    }
}
