use std::ops::Range;

const OSC11_REPLY_PREFIX: &[u8] = b"\x1b]11;";
const OSC11_RGB_PREFIX: &[u8] = b"\x1b]11;rgb:";
const PRIMARY_DEVICE_ATTRIBUTES_PREFIX: &[u8] = b"\x1b[?";
const LIGHT_LUMINANCE_THRESHOLD: u32 = 32768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalBackground {
    pub(crate) light: bool,
    pub(crate) rgb: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseStatus {
    Invalid,
    Pending,
    Complete,
}

pub(crate) fn classify_primary_device_attributes(bytes: &[u8]) -> ResponseStatus {
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

pub(crate) fn trailing_primary_device_attributes(bytes: &[u8]) -> Option<usize> {
    let start = bytes
        .windows(PRIMARY_DEVICE_ATTRIBUTES_PREFIX.len())
        .rposition(|window| window == PRIMARY_DEVICE_ATTRIBUTES_PREFIX)?;
    (classify_primary_device_attributes(&bytes[start..]) == ResponseStatus::Complete)
        .then_some(start)
}

pub(crate) fn find_osc11_reply(bytes: &[u8]) -> Option<Range<usize>> {
    let start = bytes
        .windows(OSC11_REPLY_PREFIX.len())
        .position(|window| window == OSC11_REPLY_PREFIX)?;
    let body = &bytes[start + OSC11_REPLY_PREFIX.len()..];
    let bell = body
        .iter()
        .position(|byte| *byte == 0x07)
        .map(|index| index + 1);
    let string_terminator = body
        .windows(2)
        .position(|window| window == b"\x1b\\")
        .map(|index| index + 2);
    let length = match (bell, string_terminator) {
        (Some(bell), Some(terminator)) => bell.min(terminator),
        (bell, terminator) => bell.or(terminator)?,
    };
    Some(start..start + OSC11_REPLY_PREFIX.len() + length)
}

pub(crate) fn parse_osc11_response(bytes: &[u8]) -> Option<TerminalBackground> {
    let body = bytes.strip_prefix(OSC11_RGB_PREFIX)?;
    let body = body
        .strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x07"))?;
    if body.is_empty() {
        return None;
    }

    let mut parts = body.split(|byte| *byte == b'/');
    let mut components = [0_u32; 3];
    for component in &mut components {
        *component = normalize_osc11_component(parts.next()?)?;
    }
    if parts.next().is_some() {
        return None;
    }

    let [r, g, b] = components;
    let luminance = (r * 299 + g * 587 + b * 114) / 1000;
    Some(TerminalBackground {
        light: luminance > LIGHT_LUMINANCE_THRESHOLD,
        rgb: Rgb {
            r: high_byte(r),
            g: high_byte(g),
            b: high_byte(b),
        },
    })
}

pub(crate) fn truecolor_supported_for_values(
    colorterm: Option<&str>,
    term_program: Option<&str>,
) -> bool {
    if let Some(value) = colorterm
        && (value.contains("truecolor") || value.contains("24bit"))
    {
        return true;
    }
    term_program != Some("Apple_Terminal")
}

pub(crate) fn parse_color_fg_bg_light(colorfgbg: &str) -> bool {
    let Some((_, background)) = colorfgbg.rsplit_once(';') else {
        return false;
    };
    parse_unsigned(background.as_bytes(), 10)
        .and_then(|index| u8::try_from(index).ok())
        .is_some_and(|index| index >= 8)
}

fn parse_unsigned(digits: &[u8], radix: u32) -> Option<u32> {
    if digits.is_empty() || digits.starts_with(b"_") || digits.ends_with(b"_") {
        return None;
    }
    digits
        .iter()
        .filter(|byte| **byte != b'_')
        .try_fold(0_u32, |value, byte| {
            let digit = char::from(*byte).to_digit(radix)?;
            value.checked_mul(radix)?.checked_add(digit)
        })
}

fn normalize_osc11_component(part: &[u8]) -> Option<u32> {
    if part.is_empty() || part.len() > 4 {
        return None;
    }
    let value = parse_unsigned(part, 16)?;
    let maximum = (1_u32 << (part.len() * 4)) - 1;
    Some(value * 0xffff / maximum)
}

fn high_byte(component: u32) -> u8 {
    u8::try_from(component >> 8).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_supported_for_values_honors_colorterm_over_program_name() {
        assert!(truecolor_supported_for_values(
            Some("truecolor"),
            Some("Apple_Terminal")
        ));
    }

    #[test]
    fn truecolor_supported_for_values_degrades_apple_terminal_without_colorterm() {
        assert!(!truecolor_supported_for_values(
            None,
            Some("Apple_Terminal")
        ));
    }

    #[test]
    fn truecolor_supported_for_values_defaults_to_truecolor_for_unknown_terminals() {
        assert!(truecolor_supported_for_values(None, None));
        assert!(truecolor_supported_for_values(None, Some("ghostty")));
    }

    #[test]
    fn the_device_attributes_fence_ends_the_background_probe() {
        let dark = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\\x1b[?62;22c";
        let fence = trailing_primary_device_attributes(dark).unwrap();
        assert!(!parse_osc11_response(&dark[..fence]).unwrap().light);
        let light = b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?1;2c";
        let fence = trailing_primary_device_attributes(light).unwrap();
        assert!(parse_osc11_response(&light[..fence]).unwrap().light);
        assert_eq!(trailing_primary_device_attributes(b"\x1b[?62;22c"), Some(0));
    }

    #[test]
    fn the_background_probe_keeps_reading_until_the_fence_completes() {
        for pending in [
            &b"\x1b]11;rgb:cccc/cccc/cc"[..],
            b"\x1b]11;rgb:cccc/cccc/cccc\x1b\\",
            b"\x1b]11;rgb:cccc/cccc/cccc\x1b\\\x1b[?62;2",
            b"\x1b[?c",
            b"\x1b[?62;c",
            b"",
        ] {
            assert_eq!(
                trailing_primary_device_attributes(pending),
                None,
                "{pending:?}"
            );
        }
    }

    #[test]
    fn primary_device_attributes_replies_are_classified_as_upstream_does() {
        for (bytes, status) in [
            (&b"\x1b"[..], ResponseStatus::Pending),
            (b"\x1b[?", ResponseStatus::Pending),
            (b"\x1b[?62;22", ResponseStatus::Pending),
            (b"\x1b[?62;22c", ResponseStatus::Complete),
            (b"\x1b[?1;2c", ResponseStatus::Complete),
            (b"\x1b[?c", ResponseStatus::Invalid),
            (b"\x1b[?62;;1c", ResponseStatus::Invalid),
            (b"\x1b[?62;c", ResponseStatus::Invalid),
            (b"\x1b[?62cx", ResponseStatus::Invalid),
            (b"\x1b[62c", ResponseStatus::Invalid),
        ] {
            assert_eq!(
                classify_primary_device_attributes(bytes),
                status,
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn osc_11_replies_are_located_among_surrounding_input() {
        assert_eq!(find_osc11_reply(b"x\x1b]11;rgb:1/2/3\x1b\\y"), Some(1..17));
        assert_eq!(find_osc11_reply(b"\x1b]11;rgb:1/2/3\x07"), Some(0..15));
        assert_eq!(find_osc11_reply(b"\\\x1b]11;rgb:1/2"), None);
        assert_eq!(find_osc11_reply(b"typed\\"), None);
    }

    #[test]
    fn colorfgbg_parser_detects_light_terminal() {
        assert!(parse_color_fg_bg_light("0;15"));
        assert!(parse_color_fg_bg_light("0;8"));
        assert!(parse_color_fg_bg_light("1;3;15"));
        assert!(!parse_color_fg_bg_light("15;0"));
        assert!(!parse_color_fg_bg_light("0;7"));
        assert!(!parse_color_fg_bg_light(""));
        assert!(!parse_color_fg_bg_light("garbage"));
    }

    #[test]
    fn colorfgbg_parser_reads_the_background_as_upstream_parses_an_unsigned_byte() {
        assert!(parse_color_fg_bg_light("0;1_5"));
        assert!(parse_color_fg_bg_light("0;1__5"));
        assert!(parse_color_fg_bg_light("0;0015"));
        assert!(parse_color_fg_bg_light("0;255"));
        assert!(!parse_color_fg_bg_light("0;+15"));
        assert!(!parse_color_fg_bg_light("0;_15"));
        assert!(!parse_color_fg_bg_light("0;15_"));
        assert!(!parse_color_fg_bg_light("0;256"));
        assert!(!parse_color_fg_bg_light("0;"));
        assert!(!parse_color_fg_bg_light("0;1 5"));
    }

    #[test]
    fn osc_11_parser_detects_light_background_and_extracts_rgb() {
        let white = parse_osc11_response(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\").unwrap();
        assert!(white.light);
        assert_eq!(white.rgb.r, 0xff);

        let near_white = parse_osc11_response(b"\x1b]11;rgb:f0f0/f0f0/f0f0\x07").unwrap();
        assert!(near_white.light);

        let black = parse_osc11_response(b"\x1b]11;rgb:0000/0000/0000\x1b\\").unwrap();
        assert!(!black.light);
        assert_eq!(black.rgb.r, 0x00);

        let dark = parse_osc11_response(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\").unwrap();
        assert!(!dark.light);
        assert_eq!(dark.rgb.r, 0x1c);

        assert_eq!(parse_osc11_response(b"garbage"), None);
    }

    #[test]
    fn osc_11_parser_normalizes_one_digit_rgb_channels() {
        let white = parse_osc11_response(b"\x1b]11;rgb:f/f/f\x07").unwrap();
        assert!(white.light);
        assert_eq!(
            white.rgb,
            Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff
            }
        );
    }

    #[test]
    fn osc_11_parser_supports_every_rgb_component_width() {
        let white_samples: [&[u8]; 4] = [
            b"\x1b]11;rgb:f/f/f\x07",
            b"\x1b]11;rgb:ff/ff/ff\x07",
            b"\x1b]11;rgb:fff/fff/fff\x07",
            b"\x1b]11;rgb:ffff/ffff/ffff\x07",
        ];
        let dark_samples: [&[u8]; 4] = [
            b"\x1b]11;rgb:4/4/4\x07",
            b"\x1b]11;rgb:44/44/44\x07",
            b"\x1b]11;rgb:444/444/444\x07",
            b"\x1b]11;rgb:4444/4444/4444\x07",
        ];
        for sample in white_samples {
            let parsed = parse_osc11_response(sample).unwrap();
            assert!(parsed.light);
            assert_eq!(parsed.rgb.r, 0xff);
        }
        for sample in dark_samples {
            let parsed = parse_osc11_response(sample).unwrap();
            assert!(!parsed.light);
            assert_eq!(parsed.rgb.r, 0x44);
        }
    }

    #[test]
    fn osc_11_parser_reads_components_as_upstream_parses_an_unsigned_integer() {
        let separated = parse_osc11_response(b"\x1b]11;rgb:f_f/f_f/f_f\x07").unwrap();
        assert!(!separated.light);
        assert_eq!(
            separated.rgb,
            Rgb {
                r: 0x0f,
                g: 0x0f,
                b: 0x0f
            }
        );
        let repeated = parse_osc11_response(b"\x1b]11;rgb:f__f/f__f/f__f\x1b\\").unwrap();
        assert_eq!(repeated.rgb.r, 0x00);
        let upper = parse_osc11_response(b"\x1b]11;rgb:FFFF/FFFF/FFFF\x07").unwrap();
        assert!(upper.light);
        let rejected: [&[u8]; 5] = [
            b"\x1b]11;rgb:_ff/ff/ff\x07",
            b"\x1b]11;rgb:ff_/ff/ff\x07",
            b"\x1b]11;rgb:+ff/ff/ff\x07",
            b"\x1b]11;rgb:ff_ff/ff/ff\x07",
            b"\x1b]11;rgb:f f/ff/ff\x07",
        ];
        for sample in rejected {
            assert_eq!(parse_osc11_response(sample), None);
        }
    }

    #[test]
    fn osc_11_parser_rejects_non_osc_11_envelopes() {
        let invalid: [&[u8]; 8] = [
            b"rgb:ffff/ffff/ffff",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"\x1b]11;rgb:ffff/ffff/ffff",
            b"\x1b]11;rgb:ffff/ffff/ffff/ffff\x07",
            b"\x1b]11;rgb:ffff/ffff\x07",
            b"\x1b]11;rgb:ffff/ffff/zzzz\x07",
            b"\x1b]11;rgb:fffff/ffff/ffff\x07",
            b"\x1b]11;rgb:ffff/ffff/ffff\x07tail",
        ];
        for sample in invalid {
            assert_eq!(parse_osc11_response(sample), None);
        }
    }
}
