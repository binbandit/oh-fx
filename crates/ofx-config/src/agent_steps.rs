pub(crate) const DEFAULT_MAX_AGENT_STEPS: u64 = 0;

pub(crate) fn resolve_max_agent_steps(configured: Option<u64>, default_value: u64) -> u64 {
    configured.unwrap_or(default_value)
}

const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const DIGIT_SEPARATOR: u8 = b'_';

pub(crate) fn parse_max_agent_steps(raw: Option<&str>) -> Option<u64> {
    let digits = raw?.trim_matches(TRIMMED).as_bytes();
    let (first, last) = (*digits.first()?, *digits.last()?);
    if first == DIGIT_SEPARATOR || last == DIGIT_SEPARATOR {
        return None;
    }
    digits
        .iter()
        .filter(|byte| **byte != DIGIT_SEPARATOR)
        .try_fold(0_u64, |value, byte| {
            let digit = char::from(*byte).to_digit(10)?;
            value.checked_mul(10)?.checked_add(u64::from(digit))
        })
}

pub(crate) fn resolve_max_agent_steps_with_override(
    configured: Option<u64>,
    default_value: u64,
    process_override: Option<&str>,
) -> u64 {
    parse_max_agent_steps(process_override)
        .unwrap_or_else(|| resolve_max_agent_steps(configured, default_value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_max_agent_steps_preserves_explicit_unbounded_zero() {
        assert_eq!(resolve_max_agent_steps(None, 25), 25);
        assert_eq!(resolve_max_agent_steps(Some(0), 25), 0);
        assert_eq!(resolve_max_agent_steps(Some(50), 25), 50);
    }

    #[test]
    fn parse_max_agent_steps_accepts_zero_and_rejects_invalid_input() {
        assert_eq!(parse_max_agent_steps(Some("0")), Some(0));
        assert_eq!(parse_max_agent_steps(Some("24")), Some(24));
        assert_eq!(parse_max_agent_steps(None), None);
        assert_eq!(parse_max_agent_steps(Some("")), None);
        assert_eq!(parse_max_agent_steps(Some("abc")), None);
    }

    #[test]
    fn process_overrides_resolve_with_configured_and_compiled_defaults() {
        assert_eq!(
            resolve_max_agent_steps_with_override(None, DEFAULT_MAX_AGENT_STEPS, None),
            0
        );
        assert_eq!(resolve_max_agent_steps_with_override(Some(0), 24, None), 0);
        assert_eq!(
            resolve_max_agent_steps_with_override(None, 0, Some("24")),
            24
        );
        assert_eq!(
            resolve_max_agent_steps_with_override(Some(24), 8, Some("0")),
            0
        );
        assert_eq!(
            resolve_max_agent_steps_with_override(Some(24), 8, Some("invalid")),
            24
        );
        assert_eq!(
            resolve_max_agent_steps_with_override(Some(24), 8, Some("  \t\n")),
            24
        );
    }

    #[test]
    fn overrides_read_numbers_as_upstreams_unsigned_parse_does() {
        assert_eq!(parse_max_agent_steps(Some(" \t12\r\n")), Some(12));
        assert_eq!(parse_max_agent_steps(Some("1_000")), Some(1000));
        assert_eq!(parse_max_agent_steps(Some("1__0")), Some(10));
        for rejected in [
            "_1",
            "1_",
            "+5",
            "-5",
            "\u{a0}5",
            "5\u{a0}",
            "0x10",
            "18446744073709551616",
        ] {
            assert_eq!(parse_max_agent_steps(Some(rejected)), None, "{rejected:?}");
        }
        assert_eq!(
            parse_max_agent_steps(Some("18446744073709551615")),
            Some(u64::MAX)
        );
    }
}
