use ofx_text::parse_unsigned;

const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactPercent(u8);

impl AutoCompactPercent {
    const DEFAULT: Self = Self(80);
    const MIN: u8 = 10;
    const MAX: u8 = 80;

    pub fn new(value: u64) -> Option<Self> {
        u8::try_from(value)
            .ok()
            .filter(|value| (Self::MIN..=Self::MAX).contains(value))
            .map(Self)
    }

    pub fn resolve(configured: Option<Self>, process_override: Option<&str>) -> Self {
        process_override
            .and_then(Self::parse)
            .or(configured)
            .unwrap_or(Self::DEFAULT)
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    fn parse(raw: &str) -> Option<Self> {
        parse_unsigned::<u8>(raw.trim_matches(TRIMMED)).and_then(|value| Self::new(value.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_compaction_percent_accepts_only_the_supported_range() {
        assert_eq!(AutoCompactPercent::new(9), None);
        assert_eq!(
            AutoCompactPercent::new(10).map(AutoCompactPercent::get),
            Some(10)
        );
        assert_eq!(
            AutoCompactPercent::new(80).map(AutoCompactPercent::get),
            Some(80)
        );
        assert_eq!(AutoCompactPercent::new(81), None);
        assert_eq!(AutoCompactPercent::new(266), None);
        assert_eq!(
            AutoCompactPercent::parse(" 50\n").map(AutoCompactPercent::get),
            Some(50)
        );
        assert_eq!(
            AutoCompactPercent::parse("5_0").map(AutoCompactPercent::get),
            Some(50)
        );
        for invalid in ["5", "90", "half", "", "+50", "306"] {
            assert_eq!(AutoCompactPercent::parse(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn auto_compaction_percent_resolves_override_then_setting_then_default() {
        let forty = AutoCompactPercent::new(40);
        assert_eq!(AutoCompactPercent::resolve(None, None).get(), 80);
        assert_eq!(AutoCompactPercent::resolve(forty, None).get(), 40);
        assert_eq!(AutoCompactPercent::resolve(forty, Some("25")).get(), 25);
        assert_eq!(AutoCompactPercent::resolve(forty, Some("95")).get(), 40);
    }
}
