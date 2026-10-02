use unicode_segmentation::UnicodeSegmentation;

use super::{MAX_RGI_SEQUENCE_CODEPOINTS, RGI_EMOJI_SEQUENCES, match_rgi_sequence};

struct Xorshift(u64);

impl Xorshift {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
    }
}

fn sequence_list() -> Vec<&'static str> {
    RGI_EMOJI_SEQUENCES.lines().collect()
}

fn listed_prefix(sequences: &[&str], first: &str) -> bool {
    let position = sequences.partition_point(|sequence| *sequence < first);
    sequences
        .get(position)
        .is_some_and(|sequence| sequence.starts_with(first))
}

fn listed_match(sequences: &[&str], rest: &str) -> usize {
    let Some(first) = rest.chars().next() else {
        return 0;
    };
    if !listed_prefix(sequences, &rest[..first.len_utf8()]) {
        return 0;
    }
    let window_end = rest
        .char_indices()
        .nth(MAX_RGI_SEQUENCE_CODEPOINTS)
        .map_or(rest.len(), |(offset, _)| offset);
    let Some(cluster) = rest[..window_end].graphemes(true).next() else {
        return 0;
    };
    let mut end = cluster.len();
    while end > first.len_utf8() {
        let candidate = &cluster[..end];
        if sequences.binary_search(&candidate).is_ok() {
            return end;
        }
        end = candidate
            .char_indices()
            .next_back()
            .map_or(0, |(offset, _)| offset);
    }
    0
}

fn assert_same_lookups(sequences: &[&str], text: &str) {
    for (offset, _) in text.char_indices() {
        let rest = &text[offset..];
        assert_eq!(
            match_rgi_sequence(rest),
            listed_match(sequences, rest),
            "{rest:?}"
        );
    }
    for (end, codepoint) in text.char_indices() {
        let prefix = &text[..end + codepoint.len_utf8()];
        assert_eq!(
            RGI_EMOJI_SEQUENCES.contains(prefix),
            sequences.binary_search(&prefix).is_ok(),
            "{prefix:?}"
        );
    }
}

#[test]
fn every_scalar_starts_a_sequence_exactly_when_the_list_says_so() {
    let sequences = sequence_list();
    let mut buffer = [0; 4];
    for codepoint in '\0'..=char::MAX {
        assert_eq!(
            RGI_EMOJI_SEQUENCES.has_line_starting_with(codepoint),
            listed_prefix(&sequences, codepoint.encode_utf8(&mut buffer)),
            "{codepoint:?}"
        );
    }
}

#[test]
fn every_ascii_character_with_a_selector_or_keycap_matches_like_the_list() {
    let sequences = sequence_list();
    for byte in 0..=0x7f_u8 {
        let base = char::from(byte);
        for tail in [
            "",
            "\u{fe0f}",
            "\u{fe0e}",
            "\u{20e3}",
            "\u{fe0f}\u{20e3}",
            "\u{fe0e}\u{20e3}",
        ] {
            assert_same_lookups(&sequences, &format!("{base}{tail}"));
        }
    }
}

#[test]
fn every_sequence_its_prefixes_and_its_continuations_match_like_the_list() {
    let sequences = sequence_list();
    assert_eq!(sequences.len(), 2553);
    for sequence in &sequences {
        assert!(RGI_EMOJI_SEQUENCES.contains(sequence), "{sequence:?}");
        for tail in [
            "",
            "x",
            " ",
            "\u{200d}",
            "\u{fe0e}",
            "\u{fe0f}",
            "\u{20e3}",
            "\u{1f3fb}",
            "\u{1f3ff}",
            "\u{1f1fa}",
            "\u{e007f}",
            "\u{200d}\u{1f466}",
            "\u{2764}\u{fe0f}",
        ] {
            assert_same_lookups(&sequences, &format!("{sequence}{tail}"));
        }
    }
}

#[test]
fn random_emoji_clusters_match_like_the_list() {
    let sequences = sequence_list();
    let mut alphabet: Vec<char> = sequences
        .iter()
        .flat_map(|sequence| sequence.chars())
        .collect();
    alphabet.sort_unstable();
    alphabet.dedup();
    alphabet.extend([
        'a',
        '#',
        '*',
        '0',
        '9',
        ' ',
        '\u{301}',
        '\u{4e2d}',
        '\u{fe0e}',
        '\u{1f3fd}',
    ]);
    let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
    for _ in 0..50_000 {
        let mut text = String::new();
        let mut sequence = sequences[rng.below(sequences.len())].chars();
        let cut = rng.below(MAX_RGI_SEQUENCE_CODEPOINTS);
        text.extend(sequence.by_ref().take(cut));
        for _ in 0..rng.below(12) {
            if rng.below(16) == 0 {
                text.extend(char::from_u32(
                    u32::try_from(rng.below(0x11_0000)).unwrap_or(0),
                ));
            } else {
                text.push(alphabet[rng.below(alphabet.len())]);
            }
        }
        text.extend(sequence);
        assert_same_lookups(&sequences, &text);
    }
}
