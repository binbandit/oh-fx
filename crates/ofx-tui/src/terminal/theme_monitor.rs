use super::forwarded_bytes::ForwardedBytes;
use super::theme_protocol::{Rgb, parse_osc11_response};

const RESPONSE_IDLE_TIMEOUT_MS: i64 = 75;
const BACKGROUND_RESPONSE_TIMEOUT_MS: i64 = 200;

const MAX_CANDIDATE_BYTES: usize = 64;
const DARK_RESPONSE: &[u8] = b"\x1b[?997;1n";
const LIGHT_RESPONSE: &[u8] = b"\x1b[?997;2n";
const PRIMARY_DEVICE_ATTRIBUTES_PREFIX: &[u8] = b"\x1b[?";
const OSC11_PREFIX: &[u8] = b"\x1b]11;rgb:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThemeUpdate {
    pub(crate) light: bool,
    pub(crate) rgb: Option<Rgb>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeQuery {
    ResponseFence,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedResult {
    Pending,
    Consumed,
    Forward(ForwardedBytes),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum QueryState {
    #[default]
    Idle,
    AwaitingResponseFence {
        deadline_ms: i64,
    },
    BackgroundReady,
    AwaitingBackground {
        deadline_ms: i64,
        background: Option<ThemeUpdate>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseStatus {
    Invalid,
    Pending,
    Complete,
}

#[derive(Debug, Default)]
pub(crate) struct Monitor {
    pub(crate) enabled: bool,
    candidate: Vec<u8>,
    candidate_deadline_ms: i64,
    deferred: Vec<u8>,
    deferred_start: usize,
    query_state: QueryState,
    theme_dirty: bool,
    notification_light: Option<bool>,
    settled_update: Option<ThemeUpdate>,
}

impl Monitor {
    pub(crate) fn start(&mut self) {
        self.enabled = true;
    }

    pub(crate) fn has_pending_input(&self) -> bool {
        !self.candidate.is_empty() || self.deferred_start < self.deferred.len()
    }

    pub(crate) fn feed(&mut self, byte: u8, now_ms: i64) -> FeedResult {
        if !self.enabled || (self.candidate.is_empty() && byte != 0x1b) {
            return FeedResult::Forward(ForwardedBytes::single(byte));
        }
        if self.candidate.len() == 1 && byte == 0x1b {
            self.candidate_deadline_ms = add_millis(now_ms, RESPONSE_IDLE_TIMEOUT_MS);
            return FeedResult::Forward(ForwardedBytes::single(byte));
        }
        if self.candidate.len() == MAX_CANDIDATE_BYTES {
            let mut forwarded = self.take_candidate();
            forwarded.push(byte);
            return FeedResult::Forward(forwarded);
        }

        self.candidate.push(byte);
        self.candidate_deadline_ms = add_millis(now_ms, RESPONSE_IDLE_TIMEOUT_MS);

        if self.candidate == DARK_RESPONSE || self.candidate == LIGHT_RESPONSE {
            let light = self.candidate == LIGHT_RESPONSE;
            self.candidate.clear();
            self.queue_refresh(light);
            return FeedResult::Consumed;
        }
        if DARK_RESPONSE.starts_with(&self.candidate) || LIGHT_RESPONSE.starts_with(&self.candidate)
        {
            return FeedResult::Pending;
        }

        match classify_primary_device_attributes(&self.candidate) {
            ResponseStatus::Pending => return FeedResult::Pending,
            ResponseStatus::Complete => {
                self.candidate.clear();
                self.candidate_deadline_ms = 0;
                self.finish_response_fence();
                return FeedResult::Consumed;
            }
            ResponseStatus::Invalid => {}
        }

        if OSC11_PREFIX.starts_with(&self.candidate) {
            return FeedResult::Pending;
        }
        if self.candidate.starts_with(OSC11_PREFIX) {
            if !self.candidate.ends_with(b"\x07") && !self.candidate.ends_with(b"\x1b\\") {
                return FeedResult::Pending;
            }
            if let Some(background) = parse_osc11_response(&self.candidate) {
                self.candidate.clear();
                self.candidate_deadline_ms = 0;
                if let QueryState::AwaitingBackground {
                    background: sample, ..
                } = &mut self.query_state
                {
                    *sample = Some(ThemeUpdate {
                        light: background.light,
                        rgb: Some(background.rgb),
                    });
                }
                return FeedResult::Consumed;
            }
        }
        FeedResult::Forward(self.take_candidate())
    }

    pub(crate) fn poll(&mut self, now_ms: i64) {
        if !self.candidate.is_empty() && now_ms >= self.candidate_deadline_ms {
            if self.candidate.starts_with(OSC11_PREFIX) {
                self.candidate.clear();
                self.candidate_deadline_ms = 0;
            } else {
                self.defer_candidate();
            }
        }
        let query_deadline = match self.query_state {
            QueryState::AwaitingResponseFence { deadline_ms }
            | QueryState::AwaitingBackground { deadline_ms, .. } => deadline_ms,
            QueryState::Idle | QueryState::BackgroundReady => return,
        };
        if now_ms >= query_deadline {
            self.settle_notification_fallback();
        }
    }

    pub(crate) fn take_query_request(&mut self, now_ms: i64) -> Option<ThemeQuery> {
        match self.query_state {
            QueryState::Idle => {
                if !self.theme_dirty {
                    return None;
                }
                self.query_state = QueryState::AwaitingResponseFence {
                    deadline_ms: add_millis(now_ms, BACKGROUND_RESPONSE_TIMEOUT_MS),
                };
                Some(ThemeQuery::ResponseFence)
            }
            QueryState::BackgroundReady => {
                self.theme_dirty = false;
                self.query_state = QueryState::AwaitingBackground {
                    deadline_ms: add_millis(now_ms, BACKGROUND_RESPONSE_TIMEOUT_MS),
                    background: None,
                };
                Some(ThemeQuery::Background)
            }
            QueryState::AwaitingResponseFence { .. } | QueryState::AwaitingBackground { .. } => {
                None
            }
        }
    }

    pub(crate) fn fail_query(&mut self) {
        if self.query_state == QueryState::Idle {
            return;
        }
        self.settle_notification_fallback();
    }

    pub(crate) fn take_settled_update(&mut self) -> Option<ThemeUpdate> {
        self.settled_update.take()
    }

    pub(crate) fn take_deferred_byte(&mut self) -> Option<u8> {
        let byte = *self.deferred.get(self.deferred_start)?;
        self.deferred_start += 1;
        if self.deferred_start == self.deferred.len() {
            self.deferred.clear();
            self.deferred_start = 0;
        }
        Some(byte)
    }

    fn queue_refresh(&mut self, light: bool) {
        self.settled_update = None;
        self.notification_light = Some(light);
        self.theme_dirty = true;
    }

    fn finish_response_fence(&mut self) {
        match self.query_state {
            QueryState::AwaitingResponseFence { .. } => {
                self.query_state = QueryState::BackgroundReady;
            }
            QueryState::AwaitingBackground { background, .. } => {
                let Some(update) = background else {
                    return;
                };
                self.query_state = QueryState::Idle;
                if self.theme_dirty {
                    return;
                }
                self.notification_light = None;
                self.settled_update = Some(update);
            }
            QueryState::Idle | QueryState::BackgroundReady => {}
        }
    }

    fn settle_notification_fallback(&mut self) {
        self.query_state = QueryState::Idle;
        self.theme_dirty = false;
        if let Some(light) = self.notification_light.take() {
            self.settled_update = Some(ThemeUpdate { light, rgb: None });
        }
    }

    fn defer_candidate(&mut self) {
        if self.candidate.is_empty() {
            return;
        }
        debug_assert_eq!(self.deferred_start, self.deferred.len());
        self.deferred = std::mem::take(&mut self.candidate);
        self.deferred_start = 0;
        self.candidate_deadline_ms = 0;
    }

    fn take_candidate(&mut self) -> ForwardedBytes {
        self.candidate_deadline_ms = 0;
        let forwarded = ForwardedBytes::from_slice(&self.candidate);
        self.candidate.clear();
        forwarded
    }
}

fn classify_primary_device_attributes(bytes: &[u8]) -> ResponseStatus {
    if PRIMARY_DEVICE_ATTRIBUTES_PREFIX.starts_with(bytes) {
        return ResponseStatus::Pending;
    }
    let Some(parameters) = bytes.strip_prefix(PRIMARY_DEVICE_ATTRIBUTES_PREFIX) else {
        return ResponseStatus::Invalid;
    };
    let mut expect_digit = true;
    for (index, byte) in parameters.iter().enumerate() {
        match byte {
            b'0'..=b'9' => expect_digit = false,
            b';' if !expect_digit => expect_digit = true,
            b'c' if !expect_digit && index + 1 == parameters.len() => {
                return ResponseStatus::Complete;
            }
            _ => return ResponseStatus::Invalid,
        }
    }
    ResponseStatus::Pending
}

fn add_millis(now_ms: i64, duration_ms: i64) -> i64 {
    now_ms.saturating_add(duration_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE_FENCE: &[u8] = b"\x1b[?1;2c";

    fn feed_all(monitor: &mut Monitor, bytes: &[u8], now_ms: i64) {
        for byte in bytes {
            monitor.feed(*byte, now_ms);
        }
    }

    fn begin_background_sample(monitor: &mut Monitor, now_ms: i64) {
        assert_eq!(
            monitor.take_query_request(now_ms),
            Some(ThemeQuery::ResponseFence)
        );
        feed_all(monitor, RESPONSE_FENCE, now_ms + 1);
        assert_eq!(
            monitor.take_query_request(now_ms + 2),
            Some(ThemeQuery::Background)
        );
    }

    fn started() -> Monitor {
        let mut monitor = Monitor::default();
        monitor.start();
        monitor
    }

    fn never_forwards(result: &FeedResult) -> bool {
        matches!(result, FeedResult::Pending | FeedResult::Consumed)
    }

    #[test]
    fn theme_monitor_queues_a_fenced_background_sample_for_a_theme_notification() {
        let mut monitor = started();
        feed_all(&mut monitor, DARK_RESPONSE, 0);
        assert_eq!(
            monitor.take_query_request(0),
            Some(ThemeQuery::ResponseFence)
        );
        assert_eq!(monitor.take_query_request(0), None);
        feed_all(&mut monitor, RESPONSE_FENCE, 1);
        assert_eq!(monitor.take_query_request(2), Some(ThemeQuery::Background));
        assert_eq!(monitor.take_query_request(2), None);
    }

    #[test]
    fn theme_monitor_settles_a_strict_osc_11_response() {
        let mut monitor = started();
        feed_all(&mut monitor, LIGHT_RESPONSE, 0);
        begin_background_sample(&mut monitor, 0);
        feed_all(&mut monitor, b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\", 3);
        feed_all(&mut monitor, RESPONSE_FENCE, 4);
        let update = monitor.take_settled_update().unwrap();
        assert!(update.light);
        assert_eq!(update.rgb.unwrap().r, 0xff);
    }

    #[test]
    fn theme_monitor_trusts_terminal_background_over_os_preference() {
        let mut monitor = started();
        feed_all(&mut monitor, LIGHT_RESPONSE, 0);
        begin_background_sample(&mut monitor, 0);
        feed_all(&mut monitor, b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\", 3);
        feed_all(&mut monitor, RESPONSE_FENCE, 4);
        let update = monitor.take_settled_update().unwrap();
        assert!(!update.light);
        assert_eq!(update.rgb.unwrap().r, 0x1c);
    }

    #[test]
    fn theme_monitor_uses_the_last_background_before_the_response_fence() {
        let mut monitor = started();
        feed_all(&mut monitor, LIGHT_RESPONSE, 0);
        begin_background_sample(&mut monitor, 0);
        feed_all(&mut monitor, b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\", 3);
        feed_all(&mut monitor, b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\", 4);
        feed_all(&mut monitor, RESPONSE_FENCE, 5);
        let update = monitor.take_settled_update().unwrap();
        assert!(update.light);
        assert_eq!(update.rgb.unwrap().r, 0xff);
    }

    #[test]
    fn theme_monitor_drains_stale_backgrounds_before_sampling() {
        let mut monitor = started();
        feed_all(&mut monitor, LIGHT_RESPONSE, 0);
        assert_eq!(
            monitor.take_query_request(0),
            Some(ThemeQuery::ResponseFence)
        );
        feed_all(&mut monitor, b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\", 1);
        feed_all(&mut monitor, RESPONSE_FENCE, 2);
        assert_eq!(monitor.take_settled_update(), None);

        assert_eq!(monitor.take_query_request(3), Some(ThemeQuery::Background));
        feed_all(&mut monitor, RESPONSE_FENCE, 4);
        assert_eq!(monitor.take_settled_update(), None);
        feed_all(&mut monitor, b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\", 4);
        feed_all(&mut monitor, RESPONSE_FENCE, 5);
        let update = monitor.take_settled_update().unwrap();
        assert!(update.light);
        assert_eq!(update.rgb.unwrap().r, 0xff);
    }

    #[test]
    fn theme_monitor_settles_the_notification_after_a_background_query_timeout() {
        let mut monitor = started();
        feed_all(&mut monitor, LIGHT_RESPONSE, 0);
        begin_background_sample(&mut monitor, 0);

        let timed_out_ms = BACKGROUND_RESPONSE_TIMEOUT_MS + 2;
        monitor.poll(timed_out_ms);
        let fallback = monitor.take_settled_update().unwrap();
        assert!(fallback.light);
        assert_eq!(fallback.rgb, None);

        for byte in b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\" {
            assert!(never_forwards(
                &monitor.feed(*byte, BACKGROUND_RESPONSE_TIMEOUT_MS + 1)
            ));
        }
        assert_eq!(monitor.take_settled_update(), None);
        assert_eq!(monitor.take_query_request(timed_out_ms), None);
        assert_eq!(monitor.take_query_request(timed_out_ms + 10_000), None);
    }

    #[test]
    fn theme_monitor_settles_the_notification_after_a_query_write_failure() {
        let mut monitor = started();
        feed_all(&mut monitor, LIGHT_RESPONSE, 0);
        monitor.take_query_request(0).unwrap();
        monitor.fail_query();
        let fallback = monitor.take_settled_update().unwrap();
        assert!(fallback.light);
        assert_eq!(fallback.rgb, None);
        assert_eq!(monitor.take_query_request(10_000), None);
    }

    #[test]
    fn theme_monitor_consumes_osc_11_long_after_query_timeout_without_forwarding() {
        let mut monitor = started();
        feed_all(&mut monitor, DARK_RESPONSE, 0);
        begin_background_sample(&mut monitor, 0);
        monitor.poll(BACKGROUND_RESPONSE_TIMEOUT_MS + 2);
        assert!(!monitor.take_settled_update().unwrap().light);

        let late_ms = BACKGROUND_RESPONSE_TIMEOUT_MS * 5;
        for byte in b"\x1b]11;rgb:0000/0000/0000\x07" {
            assert!(never_forwards(&monitor.feed(*byte, late_ms)));
        }
        assert_eq!(monitor.take_settled_update(), None);
    }

    #[test]
    fn theme_monitor_forwards_a_timed_out_partial_candidate_exactly_once() {
        let mut monitor = started();
        feed_all(&mut monitor, b"\x1b[", 0);
        assert!(monitor.has_pending_input());

        monitor.poll(RESPONSE_IDLE_TIMEOUT_MS);
        let mut forwarded = Vec::new();
        for _ in 0..2 {
            forwarded.push(monitor.take_deferred_byte().unwrap());
        }
        assert_eq!(forwarded, b"\x1b[");
        assert_eq!(monitor.take_deferred_byte(), None);
        assert!(!monitor.has_pending_input());
    }

    #[test]
    fn theme_monitor_separates_user_escape_from_an_adjacent_terminal_response() {
        let mut monitor = started();
        feed_all(&mut monitor, DARK_RESPONSE, 0);
        assert_eq!(
            monitor.take_query_request(0),
            Some(ThemeQuery::ResponseFence)
        );

        assert_eq!(monitor.feed(0x1b, 1), FeedResult::Pending);
        assert_eq!(
            monitor.feed(RESPONSE_FENCE[0], 1),
            FeedResult::Forward(ForwardedBytes::single(0x1b))
        );
        feed_all(&mut monitor, &RESPONSE_FENCE[1..], 1);
        assert_eq!(monitor.take_query_request(2), Some(ThemeQuery::Background));
    }

    #[test]
    fn theme_monitor_forwards_malformed_primary_device_attributes() {
        let mut monitor = started();
        let mut forwarded = Vec::new();
        for byte in b"\x1b[?;c" {
            match monitor.feed(*byte, 0) {
                FeedResult::Forward(bytes) => forwarded.extend_from_slice(bytes.as_slice()),
                FeedResult::Pending => {}
                FeedResult::Consumed => panic!("expected the reply to be forwarded"),
            }
        }
        assert_eq!(forwarded, b"\x1b[?;c");
    }

    #[test]
    fn theme_monitor_discards_incomplete_osc_11_on_idle_timeout_with_one_trace() {
        let mut monitor = started();
        feed_all(&mut monitor, b"\x1b]11;rgb:ffff", 0);
        monitor.poll(RESPONSE_IDLE_TIMEOUT_MS);
        assert_eq!(monitor.take_deferred_byte(), None);
        assert!(!monitor.has_pending_input());
    }

    #[test]
    fn theme_monitor_idle_discard_does_not_swallow_csi_partials() {
        let mut monitor = started();
        feed_all(&mut monitor, b"\x1b[", 0);
        monitor.poll(RESPONSE_IDLE_TIMEOUT_MS);
        assert!(monitor.take_deferred_byte().is_some());
        assert!(monitor.take_deferred_byte().is_some());
    }

    #[test]
    fn theme_monitor_coalesces_a_notification_during_a_background_sample() {
        let mut monitor = started();
        feed_all(&mut monitor, DARK_RESPONSE, 0);
        begin_background_sample(&mut monitor, 0);

        feed_all(&mut monitor, LIGHT_RESPONSE, 3);
        assert_eq!(monitor.take_query_request(3), None);

        feed_all(&mut monitor, b"\x1b]11;rgb:0000/0000/0000\x07", 4);
        feed_all(&mut monitor, RESPONSE_FENCE, 5);
        assert_eq!(monitor.take_settled_update(), None);

        begin_background_sample(&mut monitor, 6);
        feed_all(&mut monitor, b"\x1b]11;rgb:ffff/ffff/ffff\x07", 9);
        feed_all(&mut monitor, RESPONSE_FENCE, 10);
        assert!(monitor.take_settled_update().unwrap().light);
    }

    #[test]
    fn theme_monitor_remains_idle_without_a_theme_notification() {
        let mut monitor = started();
        assert_eq!(monitor.take_query_request(0), None);
        assert_eq!(monitor.take_query_request(1000), None);
        assert_eq!(monitor.take_query_request(60_000), None);
    }

    #[test]
    fn theme_monitor_forwards_a_full_malformed_osc_candidate_without_dropping_its_next_byte() {
        let mut monitor = started();
        feed_all(&mut monitor, DARK_RESPONSE, 0);
        assert_eq!(
            monitor.take_query_request(0),
            Some(ThemeQuery::ResponseFence)
        );

        let mut candidate = OSC11_PREFIX.to_vec();
        candidate.resize(MAX_CANDIDATE_BYTES, b'x');
        for byte in candidate {
            assert_eq!(monitor.feed(byte, 1), FeedResult::Pending);
        }
        let FeedResult::Forward(forwarded) = monitor.feed(b'y', 1) else {
            panic!("expected the full candidate to be forwarded");
        };
        assert_eq!(forwarded.as_slice().len(), MAX_CANDIDATE_BYTES + 1);
        assert_eq!(forwarded.as_slice()[MAX_CANDIDATE_BYTES], b'y');
    }

    #[test]
    fn theme_monitor_forwards_inline_and_keeps_its_candidate_buffer() {
        let mut monitor = started();
        assert_eq!(
            monitor.feed(b'a', 0),
            FeedResult::Forward(ForwardedBytes::single(b'a'))
        );
        assert_eq!(monitor.feed(0x1b, 0), FeedResult::Pending);
        assert_eq!(monitor.feed(b'[', 0), FeedResult::Pending);
        let FeedResult::Forward(forwarded) = monitor.feed(b'x', 0) else {
            panic!("expected the invalid reply to be forwarded");
        };
        assert_eq!(forwarded.as_slice(), b"\x1b[x");
        assert!(monitor.candidate.is_empty());
        assert!(monitor.candidate.capacity() >= 3);
    }
}
