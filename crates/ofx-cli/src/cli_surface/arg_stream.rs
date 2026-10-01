use std::ffi::{OsStr, OsString};
use std::iter::Peekable;
use std::os::unix::ffi::OsStrExt;
use std::vec;

const TRIMMED_BYTES: &[u8] = b" \t\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueForm {
    Separate,
    SeparateOrJoined,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MissingValue;

pub(crate) struct ArgStream {
    args: Peekable<vec::IntoIter<OsString>>,
}

impl Iterator for ArgStream {
    type Item = OsString;

    fn next(&mut self) -> Option<OsString> {
        self.args.next()
    }
}

impl ArgStream {
    pub(crate) fn new(args: Vec<OsString>) -> Self {
        Self {
            args: args.into_iter().peekable(),
        }
    }

    pub(crate) fn peek(&mut self) -> Option<&OsStr> {
        self.args.peek().map(OsString::as_os_str)
    }

    pub(crate) fn take_flag(&mut self, flag: &str) -> bool {
        self.args.next_if(|arg| arg == flag).is_some()
    }

    pub(crate) fn take_toggle(&mut self, enable: &str, disable: &str) -> Option<bool> {
        if self.take_flag(enable) {
            Some(true)
        } else if self.take_flag(disable) {
            Some(false)
        } else {
            None
        }
    }

    pub(crate) fn take_option(
        &mut self,
        name: &str,
        form: ValueForm,
    ) -> Option<Result<OsString, MissingValue>> {
        let rest = self
            .peek()?
            .as_bytes()
            .strip_prefix(b"--")?
            .strip_prefix(name.as_bytes())?;
        let joined = match rest {
            [] => None,
            [b'=', value @ ..] if form == ValueForm::SeparateOrJoined => {
                Some(OsStr::from_bytes(value).to_os_string())
            }
            _ => return None,
        };
        self.args.next();
        Some(joined.or_else(|| self.args.next()).ok_or(MissingValue))
    }
}

pub(crate) fn requests_json(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "--json")
}

pub(crate) fn merge_toggle<E>(previous: Option<bool>, next: bool, conflict: E) -> Result<bool, E> {
    match previous {
        Some(value) if value != next => Err(conflict),
        _ => Ok(next),
    }
}

pub(crate) fn non_blank(value: &OsStr) -> Option<&OsStr> {
    let bytes = value.as_bytes();
    let start = bytes
        .iter()
        .position(|byte| !TRIMMED_BYTES.contains(byte))?;
    let end = bytes
        .iter()
        .rposition(|byte| !TRIMMED_BYTES.contains(byte))?;
    Some(OsStr::from_bytes(&bytes[start..=end]))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn stream(args: &[&str]) -> ArgStream {
        ArgStream::new(args.iter().map(OsString::from).collect())
    }

    fn bytes(raw: &[u8]) -> OsString {
        OsString::from_vec(raw.to_vec())
    }

    fn taken(
        value: Option<Result<OsString, MissingValue>>,
    ) -> Option<Result<OsString, &'static str>> {
        value.map(|value| value.map_err(|MissingValue| "missing"))
    }

    #[test]
    fn options_take_separate_values_even_when_they_look_like_flags() {
        let mut args = stream(&["--model", "--fast", "rest"]);
        assert_eq!(
            taken(args.take_option("model", ValueForm::Separate)),
            Some(Ok(OsString::from("--fast")))
        );
        assert_eq!(args.collect::<Vec<_>>(), vec![OsString::from("rest")]);
    }

    #[test]
    fn joined_values_are_accepted_only_when_requested() {
        let mut args = stream(&["--model=x"]);
        assert!(args.take_option("model", ValueForm::Separate).is_none());
        assert_eq!(
            taken(args.take_option("model", ValueForm::SeparateOrJoined)),
            Some(Ok(OsString::from("x")))
        );
        assert_eq!(args.peek(), None);
    }

    #[test]
    fn options_match_whole_names_and_report_missing_values() {
        let mut args = stream(&["--model-id", "x"]);
        assert!(
            args.take_option("model", ValueForm::SeparateOrJoined)
                .is_none()
        );
        let mut missing = stream(&["--model"]);
        assert_eq!(
            taken(missing.take_option("model", ValueForm::Separate)),
            Some(Err("missing"))
        );
    }

    #[test]
    fn options_compare_bytes_and_keep_non_utf8_values() {
        let mut args = ArgStream::new(vec![
            bytes(b"--add-dir=/tmp/\xff"),
            OsString::from("--add-dir"),
            bytes(b"\xfe"),
            bytes(b"--add-dir\xff"),
        ]);
        assert_eq!(
            taken(args.take_option("add-dir", ValueForm::SeparateOrJoined)),
            Some(Ok(bytes(b"/tmp/\xff")))
        );
        assert_eq!(
            taken(args.take_option("add-dir", ValueForm::SeparateOrJoined)),
            Some(Ok(bytes(b"\xfe")))
        );
        assert!(
            args.take_option("add-dir", ValueForm::SeparateOrJoined)
                .is_none()
        );
    }

    #[test]
    fn flags_and_terminators_stay_raw() {
        let mut args = stream(&["--", "-cr", "--json"]);
        assert!(!args.take_flag("--json"));
        assert_eq!(args.next(), Some(OsString::from("--")));
        assert!(!args.take_flag("-c"));
        assert_eq!(args.next(), Some(OsString::from("-cr")));
        assert!(args.take_flag("--json"));
        assert_eq!(args.peek(), None);
    }

    #[test]
    fn non_blank_trims_only_spaces_tabs_and_line_breaks_and_keeps_raw_bytes() {
        assert_eq!(non_blank(OsStr::new(" \tid\r\n")), Some(OsStr::new("id")));
        assert_eq!(non_blank(OsStr::new("a b")), Some(OsStr::new("a b")));
        assert_eq!(non_blank(OsStr::new("\u{c}")), Some(OsStr::new("\u{c}")));
        assert_eq!(
            non_blank(&bytes(b" m\xff ")),
            Some(bytes(b"m\xff").as_os_str())
        );
        assert_eq!(non_blank(OsStr::new(" \r\n\t")), None);
        assert_eq!(non_blank(OsStr::new("")), None);
    }
}
