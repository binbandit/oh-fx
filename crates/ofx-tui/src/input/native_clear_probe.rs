const MAX_HELD_BYTES: usize = 4096;

#[derive(Debug, Default)]
pub(super) struct NativeClearProbe {
    enabled: bool,
    expected_row: Option<u16>,
    held_bytes: usize,
    late_response_pending: bool,
}

impl NativeClearProbe {
    pub(super) fn start(&mut self) {
        self.enabled = true;
    }

    pub(super) fn can_begin(&self) -> bool {
        self.enabled && self.expected_row.is_none()
    }

    pub(super) fn active(&self) -> bool {
        self.expected_row.is_some()
    }

    pub(super) fn busy(&self) -> bool {
        self.active() || self.late_response_pending
    }

    pub(super) fn begin(&mut self, expected_row: u16) {
        debug_assert!(self.can_begin() && expected_row > 0);
        self.expected_row = Some(expected_row);
        self.held_bytes = 1;
        self.late_response_pending = false;
    }

    pub(super) fn can_hold(&self, len: usize) -> bool {
        len <= MAX_HELD_BYTES.saturating_sub(self.held_bytes)
    }

    pub(super) fn hold(&mut self, len: usize) {
        debug_assert!(self.active() && self.can_hold(len));
        self.held_bytes += len;
    }

    pub(super) fn settle(&mut self) -> Option<u16> {
        self.held_bytes = 0;
        self.expected_row.take()
    }

    pub(super) fn disable(&mut self, await_late_response: bool) {
        self.enabled = false;
        self.late_response_pending = await_late_response;
    }

    pub(super) fn finish_late_response(&mut self) {
        self.late_response_pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_holds_input_until_it_settles() {
        let mut probe = NativeClearProbe::default();
        assert!(!probe.can_begin());
        probe.start();
        probe.begin(17);
        probe.hold(2);
        assert!(probe.active() && probe.busy() && !probe.can_begin());
        assert_eq!(probe.settle(), Some(17));
        assert!(!probe.active() && probe.can_begin());
    }

    #[test]
    fn held_input_is_bounded() {
        let mut probe = NativeClearProbe::default();
        probe.start();
        probe.begin(17);
        probe.hold(MAX_HELD_BYTES - 1);
        assert!(!probe.can_hold(1));
        assert!(probe.can_hold(0));
    }

    #[test]
    fn disabling_can_wait_for_a_late_response() {
        let mut probe = NativeClearProbe::default();
        probe.start();
        probe.disable(true);
        assert!(!probe.can_begin() && probe.busy());
        probe.finish_late_response();
        assert!(!probe.busy());
    }
}
