use super::{KeywordCase, PROFILES, Words};

struct Xorshift(u64);

impl Xorshift {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
    }
}

fn scanned(words: &Words, token: &str, case: KeywordCase) -> bool {
    words.iter().any(|word| match case {
        KeywordCase::Sensitive => token.as_bytes() == word,
        KeywordCase::AsciiInsensitive => token.as_bytes().eq_ignore_ascii_case(word),
    })
}

fn word_lists() -> Vec<(&'static Words, KeywordCase)> {
    PROFILES
        .iter()
        .flat_map(|profile| {
            [
                (profile.aliases, KeywordCase::AsciiInsensitive),
                (profile.keywords, profile.keyword_case),
                (profile.literals, profile.keyword_case),
                (profile.keywords, KeywordCase::Sensitive),
                (profile.keywords, KeywordCase::AsciiInsensitive),
            ]
        })
        .collect()
}

fn assert_same_answers(lists: &[(&'static Words, KeywordCase)], token: &str) {
    for &(words, case) in lists {
        assert_eq!(
            words.contains(token, case),
            scanned(words, token, case),
            "{token:?} {case:?} {words:?}"
        );
    }
}

#[test]
fn every_listed_word_and_its_near_misses_match_like_a_scan() {
    let lists = word_lists();
    let mut tokens = vec![String::new(), " ".to_owned(), "_".to_owned()];
    for (words, _) in &lists {
        for word in words.iter() {
            let word = std::str::from_utf8(word).expect("ascii words");
            let mut capitalized = word.to_owned();
            capitalized[..1].make_ascii_uppercase();
            tokens.extend([
                word.to_owned(),
                word.to_ascii_uppercase(),
                capitalized,
                word[..word.len() - 1].to_owned(),
                word[1..].to_owned(),
                format!("{word}x"),
                format!("x{word}"),
                format!("{word} {word}"),
                format!("{word}{word}"),
            ]);
        }
    }
    tokens.sort_unstable();
    tokens.dedup();
    for token in &tokens {
        assert_same_answers(&lists, token);
    }
}

#[test]
fn random_tokens_match_like_a_scan() {
    let lists = word_lists();
    let pieces = [
        "a", "e", "i", "o", "n", "r", "s", "t", "f", "l", "_", "-", "$", "0", "9", "A", "E", "T",
        "\u{e9}", "\u{130}", "\u{4e2d}", " ", "if", "in", "or", "fn", "let", "for", "def", "true",
    ];
    let mut rng = Xorshift(0x2545_f491_4f6c_dd1d);
    for _ in 0..20_000 {
        let token: String = (0..rng.below(10))
            .map(|_| pieces[rng.below(pieces.len())])
            .collect();
        assert_same_answers(&lists, &token);
    }
}
