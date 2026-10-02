use super::theme_protocol::{Rgb, TerminalBackground, parse_color_fg_bg_light};

pub(crate) const THEME_ENV: &str = "OH_FX_THEME";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThemeDetection {
    pub(crate) light: bool,
    pub(crate) rgb: Option<Rgb>,
    pub(crate) pinned: bool,
}

fn explicit_theme_override(value: Option<&str>) -> Option<bool> {
    let value = value?;
    if value.eq_ignore_ascii_case("light") {
        Some(true)
    } else if value.eq_ignore_ascii_case("dark") {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn detect_theme_with(
    theme_override: Option<&str>,
    query_background: impl FnOnce() -> Option<TerminalBackground>,
    colorfgbg: Option<&str>,
) -> ThemeDetection {
    if let Some(light) = explicit_theme_override(theme_override) {
        return ThemeDetection {
            light,
            rgb: None,
            pinned: true,
        };
    }
    if let Some(background) = query_background() {
        return ThemeDetection {
            light: background.light,
            rgb: Some(background.rgb),
            pinned: false,
        };
    }
    ThemeDetection {
        light: colorfgbg.is_some_and(parse_color_fg_bg_light),
        rgb: None,
        pinned: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_light_and_dark_skip_the_background_probe() {
        let detection = detect_theme_with(Some("LIGHT"), || unreachable!(), None);
        assert_eq!(
            detection,
            ThemeDetection {
                light: true,
                rgb: None,
                pinned: true
            }
        );
        let detection = detect_theme_with(Some("dark"), || unreachable!(), Some("0;15"));
        assert_eq!(
            detection,
            ThemeDetection {
                light: false,
                rgb: None,
                pinned: true
            }
        );
    }

    #[test]
    fn background_probe_wins_over_colorfgbg() {
        let background = TerminalBackground {
            light: false,
            rgb: Rgb { r: 1, g: 2, b: 3 },
        };
        let detection = detect_theme_with(Some("custom"), || Some(background), Some("0;15"));
        assert_eq!(
            detection,
            ThemeDetection {
                light: false,
                rgb: Some(background.rgb),
                pinned: false
            }
        );
    }

    #[test]
    fn colorfgbg_fallback_defaults_to_dark() {
        assert!(detect_theme_with(None, || None, Some("0;15")).light);
        assert!(!detect_theme_with(None, || None, Some("15;0")).light);
        assert!(!detect_theme_with(None, || None, None).light);
        assert!(!detect_theme_with(None, || None, None).pinned);
    }
}
