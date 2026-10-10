const CONTINUATION_PREFIX: &str = "Continue the turn. oh-fx hook context:\n";

pub fn continuation_message(context: &str) -> String {
    format!("{CONTINUATION_PREFIX}{context}")
}

pub fn join_visible_segments(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, _) => second.to_owned(),
        (false, true) => first.to_owned(),
        (false, false) => format!("{first}\n{second}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{continuation_message, join_visible_segments};

    #[test]
    fn the_stop_continuation_uses_the_hook_prefix() {
        assert_eq!(
            continuation_message("verify the answer"),
            "Continue the turn. oh-fx hook context:\nverify the answer"
        );
    }

    #[test]
    fn visible_stop_segments_join_with_one_newline() {
        assert_eq!(join_visible_segments("first", "second"), "first\nsecond");
        assert_eq!(join_visible_segments("", "second"), "second");
        assert_eq!(join_visible_segments("first", ""), "first");
        assert_eq!(join_visible_segments("", ""), "");
    }
}
