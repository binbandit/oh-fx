pub const SESSIONS_V2_VARIABLE: &str = "OH_FX_SESSIONS_V2";

pub fn sessions_v2_variable_is_on(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_or_true_in_any_case_turns_sessions_v2_on() {
        for value in ["1", "true", "TRUE", "True"] {
            assert!(sessions_v2_variable_is_on(Some(value)), "{value:?}");
        }
        assert!(!sessions_v2_variable_is_on(None));
        for value in ["", "0", "false", "yes", "on", " 1", "1 ", "true\n", "2"] {
            assert!(!sessions_v2_variable_is_on(Some(value)), "{value:?}");
        }
    }
}
