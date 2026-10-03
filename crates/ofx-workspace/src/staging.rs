use std::ffi::OsString;
use std::fmt::Write as _;

const STAGE_PREFIX: &str = ".fx-stage-";

pub fn stage_name() -> Option<OsString> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).ok()?;
    let name = random
        .iter()
        .fold(STAGE_PREFIX.to_owned(), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        });
    Some(OsString::from(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_are_short_prefixed_and_distinct() {
        let first = stage_name().unwrap().into_string().unwrap();
        let second = stage_name().unwrap().into_string().unwrap();
        assert!(first.starts_with(STAGE_PREFIX));
        assert_eq!(first.len(), STAGE_PREFIX.len() + 32);
        assert!(
            first[STAGE_PREFIX.len()..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        assert_ne!(first, second);
    }
}
