#[derive(Clone, Copy)]
enum Step {
    Each,
    EveryOther,
}

#[derive(Clone, Copy)]
struct Run {
    first: u32,
    last: u32,
    delta: i32,
    step: Step,
}

impl Run {
    const fn new(first: u32, last: u32, delta: i32, step: Step) -> Self {
        Self {
            first,
            last,
            delta,
            step,
        }
    }
}

const RUNS: [Run; 230] = [
    Run::new(0x00B5, 0x00B5, 775, Step::Each),
    Run::new(0x00C0, 0x00D6, 32, Step::Each),
    Run::new(0x00D8, 0x00DE, 32, Step::Each),
    Run::new(0x0100, 0x012E, 1, Step::EveryOther),
    Run::new(0x0132, 0x0136, 1, Step::EveryOther),
    Run::new(0x0139, 0x0147, 1, Step::EveryOther),
    Run::new(0x014A, 0x0176, 1, Step::EveryOther),
    Run::new(0x0178, 0x0178, -121, Step::Each),
    Run::new(0x0179, 0x017D, 1, Step::EveryOther),
    Run::new(0x017F, 0x017F, -268, Step::Each),
    Run::new(0x0181, 0x0181, 210, Step::Each),
    Run::new(0x0182, 0x0182, 1, Step::Each),
    Run::new(0x0184, 0x0184, 1, Step::Each),
    Run::new(0x0186, 0x0186, 206, Step::Each),
    Run::new(0x0187, 0x0187, 1, Step::Each),
    Run::new(0x0189, 0x0189, 205, Step::Each),
    Run::new(0x018A, 0x018A, 205, Step::Each),
    Run::new(0x018B, 0x018B, 1, Step::Each),
    Run::new(0x018E, 0x018E, 79, Step::Each),
    Run::new(0x018F, 0x018F, 202, Step::Each),
    Run::new(0x0190, 0x0190, 203, Step::Each),
    Run::new(0x0191, 0x0191, 1, Step::Each),
    Run::new(0x0193, 0x0193, 205, Step::Each),
    Run::new(0x0194, 0x0194, 207, Step::Each),
    Run::new(0x0196, 0x0196, 211, Step::Each),
    Run::new(0x0197, 0x0197, 209, Step::Each),
    Run::new(0x0198, 0x0198, 1, Step::Each),
    Run::new(0x019C, 0x019C, 211, Step::Each),
    Run::new(0x019D, 0x019D, 213, Step::Each),
    Run::new(0x019F, 0x019F, 214, Step::Each),
    Run::new(0x01A0, 0x01A4, 1, Step::EveryOther),
    Run::new(0x01A6, 0x01A6, 218, Step::Each),
    Run::new(0x01A7, 0x01A7, 1, Step::Each),
    Run::new(0x01A9, 0x01A9, 218, Step::Each),
    Run::new(0x01AC, 0x01AC, 1, Step::Each),
    Run::new(0x01AE, 0x01AE, 218, Step::Each),
    Run::new(0x01AF, 0x01AF, 1, Step::Each),
    Run::new(0x01B1, 0x01B1, 217, Step::Each),
    Run::new(0x01B2, 0x01B2, 217, Step::Each),
    Run::new(0x01B3, 0x01B3, 1, Step::Each),
    Run::new(0x01B5, 0x01B5, 1, Step::Each),
    Run::new(0x01B7, 0x01B7, 219, Step::Each),
    Run::new(0x01B8, 0x01B8, 1, Step::Each),
    Run::new(0x01BC, 0x01BC, 1, Step::Each),
    Run::new(0x01C4, 0x01C4, 2, Step::Each),
    Run::new(0x01C5, 0x01C5, 1, Step::Each),
    Run::new(0x01C7, 0x01C7, 2, Step::Each),
    Run::new(0x01C8, 0x01C8, 1, Step::Each),
    Run::new(0x01CA, 0x01CA, 2, Step::Each),
    Run::new(0x01CB, 0x01DB, 1, Step::EveryOther),
    Run::new(0x01DE, 0x01EE, 1, Step::EveryOther),
    Run::new(0x01F1, 0x01F1, 2, Step::Each),
    Run::new(0x01F2, 0x01F2, 1, Step::Each),
    Run::new(0x01F4, 0x01F4, 1, Step::Each),
    Run::new(0x01F6, 0x01F6, -97, Step::Each),
    Run::new(0x01F7, 0x01F7, -56, Step::Each),
    Run::new(0x01F8, 0x021E, 1, Step::EveryOther),
    Run::new(0x0220, 0x0220, -130, Step::Each),
    Run::new(0x0222, 0x0232, 1, Step::EveryOther),
    Run::new(0x023A, 0x023A, 10795, Step::Each),
    Run::new(0x023B, 0x023B, 1, Step::Each),
    Run::new(0x023D, 0x023D, -163, Step::Each),
    Run::new(0x023E, 0x023E, 10792, Step::Each),
    Run::new(0x0241, 0x0241, 1, Step::Each),
    Run::new(0x0243, 0x0243, -195, Step::Each),
    Run::new(0x0244, 0x0244, 69, Step::Each),
    Run::new(0x0245, 0x0245, 71, Step::Each),
    Run::new(0x0246, 0x024E, 1, Step::EveryOther),
    Run::new(0x0345, 0x0345, 116, Step::Each),
    Run::new(0x0370, 0x0370, 1, Step::Each),
    Run::new(0x0372, 0x0372, 1, Step::Each),
    Run::new(0x0376, 0x0376, 1, Step::Each),
    Run::new(0x037F, 0x037F, 116, Step::Each),
    Run::new(0x0386, 0x0386, 38, Step::Each),
    Run::new(0x0388, 0x038A, 37, Step::Each),
    Run::new(0x038C, 0x038C, 64, Step::Each),
    Run::new(0x038E, 0x038E, 63, Step::Each),
    Run::new(0x038F, 0x038F, 63, Step::Each),
    Run::new(0x0391, 0x03A1, 32, Step::Each),
    Run::new(0x03A3, 0x03AB, 32, Step::Each),
    Run::new(0x03C2, 0x03C2, 1, Step::Each),
    Run::new(0x03CF, 0x03CF, 8, Step::Each),
    Run::new(0x03D0, 0x03D0, -30, Step::Each),
    Run::new(0x03D1, 0x03D1, -25, Step::Each),
    Run::new(0x03D5, 0x03D5, -15, Step::Each),
    Run::new(0x03D6, 0x03D6, -22, Step::Each),
    Run::new(0x03D8, 0x03EE, 1, Step::EveryOther),
    Run::new(0x03F0, 0x03F0, -54, Step::Each),
    Run::new(0x03F1, 0x03F1, -48, Step::Each),
    Run::new(0x03F4, 0x03F4, -60, Step::Each),
    Run::new(0x03F5, 0x03F5, -64, Step::Each),
    Run::new(0x03F7, 0x03F7, 1, Step::Each),
    Run::new(0x03F9, 0x03F9, -7, Step::Each),
    Run::new(0x03FA, 0x03FA, 1, Step::Each),
    Run::new(0x03FD, 0x03FF, -130, Step::Each),
    Run::new(0x0400, 0x040F, 80, Step::Each),
    Run::new(0x0410, 0x042F, 32, Step::Each),
    Run::new(0x0460, 0x0480, 1, Step::EveryOther),
    Run::new(0x048A, 0x04BE, 1, Step::EveryOther),
    Run::new(0x04C0, 0x04C0, 15, Step::Each),
    Run::new(0x04C1, 0x04CD, 1, Step::EveryOther),
    Run::new(0x04D0, 0x052E, 1, Step::EveryOther),
    Run::new(0x0531, 0x0556, 48, Step::Each),
    Run::new(0x10A0, 0x10C5, 7264, Step::Each),
    Run::new(0x10C7, 0x10C7, 7264, Step::Each),
    Run::new(0x10CD, 0x10CD, 7264, Step::Each),
    Run::new(0x13F8, 0x13FD, -8, Step::Each),
    Run::new(0x1C80, 0x1C80, -6222, Step::Each),
    Run::new(0x1C81, 0x1C81, -6221, Step::Each),
    Run::new(0x1C82, 0x1C82, -6212, Step::Each),
    Run::new(0x1C83, 0x1C83, -6210, Step::Each),
    Run::new(0x1C84, 0x1C84, -6210, Step::Each),
    Run::new(0x1C85, 0x1C85, -6211, Step::Each),
    Run::new(0x1C86, 0x1C86, -6204, Step::Each),
    Run::new(0x1C87, 0x1C87, -6180, Step::Each),
    Run::new(0x1C88, 0x1C88, 35267, Step::Each),
    Run::new(0x1C89, 0x1C89, 1, Step::Each),
    Run::new(0x1C90, 0x1CBA, -3008, Step::Each),
    Run::new(0x1CBD, 0x1CBF, -3008, Step::Each),
    Run::new(0x1E00, 0x1E94, 1, Step::EveryOther),
    Run::new(0x1E9B, 0x1E9B, -58, Step::Each),
    Run::new(0x1E9E, 0x1E9E, -7615, Step::Each),
    Run::new(0x1EA0, 0x1EFE, 1, Step::EveryOther),
    Run::new(0x1F08, 0x1F0F, -8, Step::Each),
    Run::new(0x1F18, 0x1F1D, -8, Step::Each),
    Run::new(0x1F28, 0x1F2F, -8, Step::Each),
    Run::new(0x1F38, 0x1F3F, -8, Step::Each),
    Run::new(0x1F48, 0x1F4D, -8, Step::Each),
    Run::new(0x1F59, 0x1F5F, -8, Step::EveryOther),
    Run::new(0x1F68, 0x1F6F, -8, Step::Each),
    Run::new(0x1F88, 0x1F8F, -8, Step::Each),
    Run::new(0x1F98, 0x1F9F, -8, Step::Each),
    Run::new(0x1FA8, 0x1FAF, -8, Step::Each),
    Run::new(0x1FB8, 0x1FB8, -8, Step::Each),
    Run::new(0x1FB9, 0x1FB9, -8, Step::Each),
    Run::new(0x1FBA, 0x1FBA, -74, Step::Each),
    Run::new(0x1FBB, 0x1FBB, -74, Step::Each),
    Run::new(0x1FBC, 0x1FBC, -9, Step::Each),
    Run::new(0x1FBE, 0x1FBE, -7173, Step::Each),
    Run::new(0x1FC8, 0x1FCB, -86, Step::Each),
    Run::new(0x1FCC, 0x1FCC, -9, Step::Each),
    Run::new(0x1FD3, 0x1FD3, -7235, Step::Each),
    Run::new(0x1FD8, 0x1FD8, -8, Step::Each),
    Run::new(0x1FD9, 0x1FD9, -8, Step::Each),
    Run::new(0x1FDA, 0x1FDA, -100, Step::Each),
    Run::new(0x1FDB, 0x1FDB, -100, Step::Each),
    Run::new(0x1FE3, 0x1FE3, -7219, Step::Each),
    Run::new(0x1FE8, 0x1FE8, -8, Step::Each),
    Run::new(0x1FE9, 0x1FE9, -8, Step::Each),
    Run::new(0x1FEA, 0x1FEA, -112, Step::Each),
    Run::new(0x1FEB, 0x1FEB, -112, Step::Each),
    Run::new(0x1FEC, 0x1FEC, -7, Step::Each),
    Run::new(0x1FF8, 0x1FF8, -128, Step::Each),
    Run::new(0x1FF9, 0x1FF9, -128, Step::Each),
    Run::new(0x1FFA, 0x1FFA, -126, Step::Each),
    Run::new(0x1FFB, 0x1FFB, -126, Step::Each),
    Run::new(0x1FFC, 0x1FFC, -9, Step::Each),
    Run::new(0x2126, 0x2126, -7517, Step::Each),
    Run::new(0x212A, 0x212A, -8383, Step::Each),
    Run::new(0x212B, 0x212B, -8262, Step::Each),
    Run::new(0x2132, 0x2132, 28, Step::Each),
    Run::new(0x2160, 0x216F, 16, Step::Each),
    Run::new(0x2183, 0x2183, 1, Step::Each),
    Run::new(0x24B6, 0x24CF, 26, Step::Each),
    Run::new(0x2C00, 0x2C2F, 48, Step::Each),
    Run::new(0x2C60, 0x2C60, 1, Step::Each),
    Run::new(0x2C62, 0x2C62, -10743, Step::Each),
    Run::new(0x2C63, 0x2C63, -3814, Step::Each),
    Run::new(0x2C64, 0x2C64, -10727, Step::Each),
    Run::new(0x2C67, 0x2C6B, 1, Step::EveryOther),
    Run::new(0x2C6D, 0x2C6D, -10780, Step::Each),
    Run::new(0x2C6E, 0x2C6E, -10749, Step::Each),
    Run::new(0x2C6F, 0x2C6F, -10783, Step::Each),
    Run::new(0x2C70, 0x2C70, -10782, Step::Each),
    Run::new(0x2C72, 0x2C72, 1, Step::Each),
    Run::new(0x2C75, 0x2C75, 1, Step::Each),
    Run::new(0x2C7E, 0x2C7E, -10815, Step::Each),
    Run::new(0x2C7F, 0x2C7F, -10815, Step::Each),
    Run::new(0x2C80, 0x2CE2, 1, Step::EveryOther),
    Run::new(0x2CEB, 0x2CEB, 1, Step::Each),
    Run::new(0x2CED, 0x2CED, 1, Step::Each),
    Run::new(0x2CF2, 0x2CF2, 1, Step::Each),
    Run::new(0xA640, 0xA66C, 1, Step::EveryOther),
    Run::new(0xA680, 0xA69A, 1, Step::EveryOther),
    Run::new(0xA722, 0xA72E, 1, Step::EveryOther),
    Run::new(0xA732, 0xA76E, 1, Step::EveryOther),
    Run::new(0xA779, 0xA779, 1, Step::Each),
    Run::new(0xA77B, 0xA77B, 1, Step::Each),
    Run::new(0xA77D, 0xA77D, -35332, Step::Each),
    Run::new(0xA77E, 0xA786, 1, Step::EveryOther),
    Run::new(0xA78B, 0xA78B, 1, Step::Each),
    Run::new(0xA78D, 0xA78D, -42280, Step::Each),
    Run::new(0xA790, 0xA790, 1, Step::Each),
    Run::new(0xA792, 0xA792, 1, Step::Each),
    Run::new(0xA796, 0xA7A8, 1, Step::EveryOther),
    Run::new(0xA7AA, 0xA7AA, -42308, Step::Each),
    Run::new(0xA7AB, 0xA7AB, -42319, Step::Each),
    Run::new(0xA7AC, 0xA7AC, -42315, Step::Each),
    Run::new(0xA7AD, 0xA7AD, -42305, Step::Each),
    Run::new(0xA7AE, 0xA7AE, -42308, Step::Each),
    Run::new(0xA7B0, 0xA7B0, -42258, Step::Each),
    Run::new(0xA7B1, 0xA7B1, -42282, Step::Each),
    Run::new(0xA7B2, 0xA7B2, -42261, Step::Each),
    Run::new(0xA7B3, 0xA7B3, 928, Step::Each),
    Run::new(0xA7B4, 0xA7C2, 1, Step::EveryOther),
    Run::new(0xA7C4, 0xA7C4, -48, Step::Each),
    Run::new(0xA7C5, 0xA7C5, -42307, Step::Each),
    Run::new(0xA7C6, 0xA7C6, -35384, Step::Each),
    Run::new(0xA7C7, 0xA7C7, 1, Step::Each),
    Run::new(0xA7C9, 0xA7C9, 1, Step::Each),
    Run::new(0xA7CB, 0xA7CB, -42343, Step::Each),
    Run::new(0xA7CC, 0xA7DA, 1, Step::EveryOther),
    Run::new(0xA7DC, 0xA7DC, -42561, Step::Each),
    Run::new(0xA7F5, 0xA7F5, 1, Step::Each),
    Run::new(0xAB70, 0xABBF, -38864, Step::Each),
    Run::new(0xFB05, 0xFB05, 1, Step::Each),
    Run::new(0xFF21, 0xFF3A, 32, Step::Each),
    Run::new(0x10400, 0x10427, 40, Step::Each),
    Run::new(0x104B0, 0x104D3, 40, Step::Each),
    Run::new(0x10570, 0x1057A, 39, Step::Each),
    Run::new(0x1057C, 0x1058A, 39, Step::Each),
    Run::new(0x1058C, 0x10592, 39, Step::Each),
    Run::new(0x10594, 0x10594, 39, Step::Each),
    Run::new(0x10595, 0x10595, 39, Step::Each),
    Run::new(0x10C80, 0x10CB2, 64, Step::Each),
    Run::new(0x10D50, 0x10D65, 32, Step::Each),
    Run::new(0x118A0, 0x118BF, 32, Step::Each),
    Run::new(0x16E40, 0x16E5F, 32, Step::Each),
    Run::new(0x16EA0, 0x16EB8, 27, Step::Each),
    Run::new(0x1E900, 0x1E921, 34, Step::Each),
];

pub(crate) fn fold(scalar: char) -> char {
    if scalar.is_ascii() {
        return scalar.to_ascii_lowercase();
    }
    let code = u32::from(scalar);
    let index = RUNS.partition_point(|run| run.first <= code);
    let Some(run) = index.checked_sub(1).map(|index| RUNS[index]) else {
        return scalar;
    };
    let in_step = match run.step {
        Step::Each => true,
        Step::EveryOther => (code - run.first) % 2 == 0,
    };
    if code > run.last || !in_step {
        return scalar;
    }
    code.checked_add_signed(run.delta)
        .and_then(char::from_u32)
        .unwrap_or(scalar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_matches_unicode_17_simple_case_folding_for_every_scalar() {
        let scalars = (0..=0x10_FFFF_u32).filter_map(char::from_u32);
        let total: u64 = scalars
            .clone()
            .map(|scalar| u64::from(u32::from(fold(scalar))))
            .sum();
        let mapped = scalars.filter(|scalar| fold(*scalar) != *scalar).count();
        assert_eq!(total, 620_503_229_363);
        assert_eq!(mapped, 1512);
    }

    #[test]
    fn folding_covers_common_and_simple_mappings_only() {
        assert_eq!(fold('A'), 'a');
        assert_eq!(fold('\u{c4}'), '\u{e4}');
        assert_eq!(fold('\u{3a3}'), '\u{3c3}');
        assert_eq!(fold('\u{3c2}'), '\u{3c3}');
        assert_eq!(fold('\u{212a}'), 'k');
        assert_eq!(fold('\u{df}'), '\u{df}');
        assert_eq!(fold('\u{130}'), '\u{130}');
        assert_eq!(fold('\u{1e921}'), '\u{1e943}');
    }
}
