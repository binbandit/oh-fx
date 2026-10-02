pub(crate) const CTRL_C_EXIT_WINDOW_MS: i64 = 3000;
pub(crate) const ESCAPE_CLEAR_WINDOW_MS: i64 = 500;
pub(crate) const ESCAPE_INTERRUPT_WINDOW_MS: i64 = 1000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct State {
    ctrl_c_exit: Option<i64>,
    escape_clear: Option<i64>,
    escape_interrupt: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PressResult {
    Armed,
    Activated,
}

impl State {
    pub(crate) fn ctrl_c_exit_armed(self) -> bool {
        self.ctrl_c_exit.is_some()
    }

    pub(crate) fn escape_clear_armed(self) -> bool {
        self.escape_clear.is_some()
    }

    pub(crate) fn escape_interrupt_armed(self) -> bool {
        self.escape_interrupt.is_some()
    }

    pub(crate) fn press_ctrl_c_exit(&mut self, now_ms: i64) -> PressResult {
        if let Some(armed_ms) = self.ctrl_c_exit
            && now_ms - armed_ms < CTRL_C_EXIT_WINDOW_MS
        {
            return PressResult::Activated;
        }
        self.ctrl_c_exit = Some(now_ms);
        PressResult::Armed
    }

    pub(crate) fn press_escape_clear(&mut self, now_ms: i64) -> PressResult {
        press_inclusive(&mut self.escape_clear, now_ms, ESCAPE_CLEAR_WINDOW_MS)
    }

    pub(crate) fn press_escape_interrupt(&mut self, now_ms: i64) -> PressResult {
        press_inclusive(
            &mut self.escape_interrupt,
            now_ms,
            ESCAPE_INTERRUPT_WINDOW_MS,
        )
    }

    pub(crate) fn disarm_ctrl_c_exit(&mut self) -> bool {
        self.ctrl_c_exit.take().is_some()
    }

    pub(crate) fn disarm_escape_clear(&mut self) -> bool {
        self.escape_clear.take().is_some()
    }

    pub(crate) fn disarm_escape_interrupt(&mut self) -> bool {
        self.escape_interrupt.take().is_some()
    }

    pub(crate) fn expire(&mut self, now_ms: i64) -> bool {
        let ctrl_c = self
            .ctrl_c_exit
            .is_some_and(|armed_ms| now_ms - armed_ms >= CTRL_C_EXIT_WINDOW_MS)
            && self.disarm_ctrl_c_exit();
        let clear = self
            .escape_clear
            .is_some_and(|armed_ms| now_ms - armed_ms > ESCAPE_CLEAR_WINDOW_MS)
            && self.disarm_escape_clear();
        let interrupt = self
            .escape_interrupt
            .is_some_and(|armed_ms| now_ms - armed_ms > ESCAPE_INTERRUPT_WINDOW_MS)
            && self.disarm_escape_interrupt();
        ctrl_c || clear || interrupt
    }

    pub(crate) fn next_expiry_ms(self) -> Option<i64> {
        [
            self.ctrl_c_exit
                .map(|armed_ms| armed_ms + CTRL_C_EXIT_WINDOW_MS),
            self.escape_clear
                .map(|armed_ms| armed_ms + ESCAPE_CLEAR_WINDOW_MS + 1),
            self.escape_interrupt
                .map(|armed_ms| armed_ms + ESCAPE_INTERRUPT_WINDOW_MS + 1),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

fn press_inclusive(armed: &mut Option<i64>, now_ms: i64, window_ms: i64) -> PressResult {
    if let Some(armed_ms) = *armed
        && now_ms - armed_ms <= window_ms
    {
        *armed = None;
        return PressResult::Activated;
    }
    *armed = Some(now_ms);
    PressResult::Armed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_exit_activates_only_strictly_inside_its_window() {
        let mut armed = State::default();
        assert_eq!(armed.press_ctrl_c_exit(100), PressResult::Armed);
        assert_eq!(armed.ctrl_c_exit, Some(100));

        let mut activated = armed;
        assert_eq!(
            activated.press_ctrl_c_exit(100 + CTRL_C_EXIT_WINDOW_MS - 1),
            PressResult::Activated
        );
        assert_eq!(activated, armed);

        let mut rearmed = armed;
        assert_eq!(
            rearmed.press_ctrl_c_exit(100 + CTRL_C_EXIT_WINDOW_MS),
            PressResult::Armed
        );
        assert_eq!(rearmed.ctrl_c_exit, Some(100 + CTRL_C_EXIT_WINDOW_MS));
    }

    #[test]
    fn escape_clear_activates_through_its_inclusive_window() {
        let mut armed = State::default();
        assert_eq!(armed.press_escape_clear(200), PressResult::Armed);
        assert_eq!(armed.escape_clear, Some(200));

        let mut activated = armed;
        assert_eq!(
            activated.press_escape_clear(200 + ESCAPE_CLEAR_WINDOW_MS),
            PressResult::Activated
        );
        assert!(!activated.escape_clear_armed());

        let mut rearmed = armed;
        assert_eq!(
            rearmed.press_escape_clear(201 + ESCAPE_CLEAR_WINDOW_MS),
            PressResult::Armed
        );
        assert_eq!(rearmed.escape_clear, Some(201 + ESCAPE_CLEAR_WINDOW_MS));
    }

    #[test]
    fn escape_interrupt_activates_through_its_inclusive_window() {
        let mut armed = State::default();
        assert_eq!(armed.press_escape_interrupt(200), PressResult::Armed);
        assert_eq!(armed.escape_interrupt, Some(200));

        let mut activated = armed;
        assert_eq!(
            activated.press_escape_interrupt(200 + ESCAPE_INTERRUPT_WINDOW_MS),
            PressResult::Activated
        );
        assert!(!activated.escape_interrupt_armed());

        let mut rearmed = armed;
        assert_eq!(
            rearmed.press_escape_interrupt(201 + ESCAPE_INTERRUPT_WINDOW_MS),
            PressResult::Armed
        );
    }

    #[test]
    fn gesture_expiry_preserves_boundary_behavior() {
        let mut ctrl_c = State::default();
        ctrl_c.press_ctrl_c_exit(300);
        assert!(!ctrl_c.clone().expire(300 + CTRL_C_EXIT_WINDOW_MS - 1));
        assert!(ctrl_c.expire(300 + CTRL_C_EXIT_WINDOW_MS));

        let mut escape = State::default();
        escape.press_escape_clear(400);
        assert!(!escape.clone().expire(400 + ESCAPE_CLEAR_WINDOW_MS));
        assert!(escape.expire(401 + ESCAPE_CLEAR_WINDOW_MS));

        let mut interrupt = State::default();
        interrupt.press_escape_interrupt(500);
        assert!(!interrupt.clone().expire(500 + ESCAPE_INTERRUPT_WINDOW_MS));
        assert!(interrupt.expire(501 + ESCAPE_INTERRUPT_WINDOW_MS));
    }

    #[test]
    fn gesture_disarm_and_reset_report_exactly_what_they_clear() {
        let mut all_three = State::default();
        all_three.press_ctrl_c_exit(10);
        all_three.press_escape_clear(20);
        all_three.press_escape_interrupt(30);

        let mut escape_cleared = all_three;
        assert!(escape_cleared.disarm_escape_clear());
        assert!(escape_cleared.ctrl_c_exit_armed());
        assert!(!escape_cleared.escape_clear_armed());
        assert!(escape_cleared.escape_interrupt_armed());

        let mut interrupt_cleared = all_three;
        assert!(interrupt_cleared.disarm_escape_interrupt());
        assert!(interrupt_cleared.ctrl_c_exit_armed());
        assert!(interrupt_cleared.escape_clear_armed());
        assert!(!interrupt_cleared.escape_interrupt_armed());
        assert!(!State::default().disarm_ctrl_c_exit());
        assert_eq!(
            all_three.next_expiry_ms(),
            Some(20 + ESCAPE_CLEAR_WINDOW_MS + 1)
        );
    }
}
