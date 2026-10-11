const SINGLE_TERMS: [&str; 36] = [
    "token",
    "secret",
    "credential",
    "credentials",
    "password",
    "passcode",
    "passphrase",
    "otp",
    "pin",
    "apikey",
    "apitoken",
    "accesstoken",
    "refreshtoken",
    "authtoken",
    "bearertoken",
    "privatekey",
    "secretkey",
    "signingkey",
    "sshkey",
    "clientsecret",
    "authorizationcode",
    "recoverycode",
    "backupcode",
    "seedphrase",
    "recoveryphrase",
    "creditcard",
    "cardnumber",
    "securitycode",
    "paymentcredential",
    "paymentcredentials",
    "bankaccount",
    "socialsecurity",
    "cvv",
    "cvc",
    "paymentcard",
    "paymenttoken",
];

const TWO_WORD_TERMS: [(&str, &str); 25] = [
    ("api", "key"),
    ("api", "token"),
    ("access", "token"),
    ("refresh", "token"),
    ("auth", "token"),
    ("bearer", "token"),
    ("private", "key"),
    ("secret", "key"),
    ("signing", "key"),
    ("ssh", "key"),
    ("client", "secret"),
    ("authorization", "code"),
    ("recovery", "code"),
    ("backup", "code"),
    ("seed", "phrase"),
    ("recovery", "phrase"),
    ("credit", "card"),
    ("card", "number"),
    ("security", "code"),
    ("payment", "credential"),
    ("payment", "credentials"),
    ("payment", "card"),
    ("payment", "token"),
    ("bank", "account"),
    ("social", "security"),
];

pub(super) fn is_secret_field(name: &str, title: Option<&str>) -> bool {
    contains_secret_term(name.as_bytes())
        || title.is_some_and(|title| contains_secret_term(title.as_bytes()))
}

fn contains_secret_term(value: &[u8]) -> bool {
    let mut cursor = 0;
    let mut previous: Option<&[u8]> = None;
    while let Some(word) = next_word(value, &mut cursor) {
        if SINGLE_TERMS
            .iter()
            .any(|term| word.eq_ignore_ascii_case(term.as_bytes()))
        {
            return true;
        }
        if let Some(prior) = previous
            && TWO_WORD_TERMS.iter().any(|(first, second)| {
                prior.eq_ignore_ascii_case(first.as_bytes())
                    && word.eq_ignore_ascii_case(second.as_bytes())
            })
        {
            return true;
        }
        previous = Some(word);
    }
    false
}

fn next_word<'a>(value: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    while value
        .get(*cursor)
        .is_some_and(|byte| !byte.is_ascii_alphanumeric())
    {
        *cursor += 1;
    }
    if *cursor == value.len() {
        return None;
    }
    let start = *cursor;
    let mut index = start + 1;
    while let Some(current) = value.get(index).copied().filter(u8::is_ascii_alphanumeric) {
        let previous = value[index - 1];
        if current.is_ascii_uppercase()
            && (previous.is_ascii_lowercase() || previous.is_ascii_digit())
        {
            *cursor = index;
            return Some(&value[start..index]);
        }
        if current.is_ascii_lowercase()
            && previous.is_ascii_uppercase()
            && index > start + 1
            && value[index - 2].is_ascii_uppercase()
        {
            *cursor = index - 1;
            return Some(&value[start..index - 1]);
        }
        index += 1;
    }
    *cursor = index;
    Some(&value[start..index])
}
