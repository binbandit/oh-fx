use ofx_text::{Profile, Script, contains_ignore_case, prose_profiles};

const MINIMUM_LETTERS: usize = 5;
const MINIMUM_UNEXPECTED_PROSE_LETTERS: usize = 8;
const LANGUAGE_SWITCH_SIGNALS: [&str; 25] = [
    "answer in ",
    "respond in ",
    "reply in ",
    "write in ",
    "speak in ",
    "translate",
    " language",
    "chinese",
    "mandarin",
    "cantonese",
    "japanese",
    "korean",
    "russian",
    "ukrainian",
    "bulgarian",
    "arabic",
    "persian",
    "farsi",
    "urdu",
    "hebrew",
    "greek",
    "hindi",
    "marathi",
    "nepali",
    "thai",
];
const ENGLISH_AUTHORITY_WORDS: [&str; 15] = [
    "the", "this", "that", "these", "those", "you", "your", "please", "what", "why", "how",
    "should", "would", "could", "will",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Accept,
    AcceptWithoutProse,
    RetryOnce,
    FailWithoutCommit,
    Undecidable,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DecisionInput {
    pub(crate) expected: Option<Script>,
    pub(crate) candidate: Profile,
    pub(crate) correction_attempted: bool,
    pub(crate) has_tool_calls: bool,
}

pub(crate) fn evidence(text: &str) -> Profile {
    let profiles = prose_profiles(text);
    let observed = profiles.all;
    let non_latin = profiles.non_latin;
    let minimum_unexpected_share = observed.letters.div_ceil(5);
    if let Some(script) = non_latin.script
        && non_latin.dominant_letters >= MINIMUM_UNEXPECTED_PROSE_LETTERS
        && non_latin.dominant_letters >= minimum_unexpected_share
    {
        return clear_evidence(script, non_latin.letters, non_latin.dominant_letters);
    }
    match observed.script {
        Some(script) if observed.letters >= MINIMUM_LETTERS => {
            clear_evidence(script, observed.letters, observed.dominant_letters)
        }
        _ => Profile {
            script: None,
            ..observed
        },
    }
}

pub(crate) fn infer_expectation(prompt: &str) -> Option<Script> {
    if LANGUAGE_SWITCH_SIGNALS
        .iter()
        .any(|signal| contains_ignore_case(prompt, signal))
    {
        return None;
    }
    let script = evidence(prompt).script?;
    (script == Script::Latin && has_english_authority_signal(prompt)).then_some(script)
}

pub(crate) fn decide(input: DecisionInput) -> Decision {
    let (Some(expected), Some(actual)) = (input.expected, input.candidate.script) else {
        return Decision::Undecidable;
    };
    if actual == expected {
        Decision::Accept
    } else if input.has_tool_calls {
        Decision::AcceptWithoutProse
    } else if input.correction_attempted {
        Decision::FailWithoutCommit
    } else {
        Decision::RetryOnce
    }
}

fn clear_evidence(script: Script, letters: usize, dominant_letters: usize) -> Profile {
    let required = (letters / 5) * 3 + ((letters % 5) * 3).div_ceil(5);
    Profile {
        script: (dominant_letters >= required).then_some(script),
        letters,
        dominant_letters,
    }
}

fn has_english_authority_signal(text: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .any(|word| {
            ENGLISH_AUTHORITY_WORDS
                .iter()
                .any(|candidate| word.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests;
