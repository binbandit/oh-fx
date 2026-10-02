use super::Composer;

impl Composer {
    pub(crate) fn clear(&mut self) {
        self.vertical.reset();
        self.edit.discard_selection();
        self.limit_rejection.clear();
        self.edit.clear();
        self.entities.pasted_blocks.clear();
        self.prompt_history.reset_navigation();
        self.edit_history.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixture::replace_text;
    use super::*;

    #[test]
    fn clearing_the_current_input_keeps_the_kill_ring_and_paste_ids() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "draft");
        composer.kill_ring.text.push_str("killed");
        composer.entities.next_paste_id = 3;

        composer.clear();

        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.kill_ring_text(), "killed");
        assert_eq!(composer.entities.next_paste_id, 3);
    }
}
