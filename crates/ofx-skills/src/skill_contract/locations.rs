use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use percent_encoding::percent_decode;

use super::{Skill, SkillDiagnostic};

const LOCATION_PREFIX: &str = "skill:";
const NAMESPACE_DIGITS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LocationError {
    #[error("InvalidSkillLocation")]
    InvalidSkillLocation,
    #[error("StaleSkillLocation")]
    StaleSkillLocation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Locations {
    pub namespace: u64,
    pub roots: Vec<PathBuf>,
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

impl Locations {
    pub fn resolve(&self, location: &str) -> Result<PathBuf, LocationError> {
        let Some(suffix) = location.strip_prefix(LOCATION_PREFIX) else {
            return Ok(PathBuf::from(location));
        };
        let suffix = suffix.as_bytes();
        if suffix.len() < NAMESPACE_DIGITS + 3 || suffix[NAMESPACE_DIGITS] != b':' {
            return Err(LocationError::InvalidSkillLocation);
        }
        let namespace = parse_integer(&suffix[..NAMESPACE_DIGITS], 16)
            .ok_or(LocationError::InvalidSkillLocation)?;
        if namespace != self.namespace {
            return Err(LocationError::StaleSkillLocation);
        }
        let rest = &suffix[NAMESPACE_DIGITS + 1..];
        let slash = rest
            .iter()
            .position(|&byte| byte == b'/')
            .ok_or(LocationError::InvalidSkillLocation)?;
        let root = parse_integer(&rest[..slash], 10)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.roots.get(index))
            .ok_or(LocationError::InvalidSkillLocation)?;
        let encoded_leaf = &rest[slash + 1..];
        if !percent_escapes_are_complete(encoded_leaf) {
            return Err(LocationError::InvalidSkillLocation);
        }
        let leaf: Vec<u8> = percent_decode(encoded_leaf).collect();
        let unsafe_leaf = leaf.is_empty()
            || leaf == b"."
            || leaf == b".."
            || leaf.iter().any(|byte| matches!(byte, b'/' | b'\\' | 0))
            || std::str::from_utf8(&leaf).is_err();
        if unsafe_leaf {
            return Err(LocationError::InvalidSkillLocation);
        }
        Ok(root.join(OsStr::from_bytes(&leaf)))
    }
}

fn percent_escapes_are_complete(encoded: &[u8]) -> bool {
    let mut position = 0;
    while position < encoded.len() {
        if encoded[position] == b'%' {
            let escape = encoded.get(position + 1..position + 3);
            if !escape.is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit)) {
                return false;
            }
            position += 2;
        }
        position += 1;
    }
    true
}

fn parse_integer(text: &[u8], radix: u32) -> Option<u64> {
    let (negative, digits) = match text.split_first() {
        Some((b'+', rest)) => (false, rest),
        Some((b'-', rest)) => (true, rest),
        _ => (false, text),
    };
    if digits.first().is_none_or(|&byte| byte == b'_') || digits.last() == Some(&b'_') {
        return None;
    }
    let mut value: u64 = 0;
    for &byte in digits.iter().filter(|&&byte| byte != b'_') {
        let digit = char::from(byte).to_digit(radix)?;
        value = value
            .checked_mul(u64::from(radix))?
            .checked_add(u64::from(digit))?;
    }
    (!negative || value == 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locations(namespace: u64, roots: &[&str]) -> Locations {
        Locations {
            namespace,
            roots: roots.iter().map(PathBuf::from).collect(),
            ..Locations::default()
        }
    }

    #[test]
    fn skill_locations_preserve_exact_roots_and_reject_stale_namespaces() {
        let locations = locations(19, &["/first", "/second"]);
        assert_eq!(
            locations.resolve("skill:0000000000000013:1/review"),
            Ok(PathBuf::from("/second/review"))
        );
        assert_eq!(
            locations.resolve("skill:0000000000000012:1/review"),
            Err(LocationError::StaleSkillLocation)
        );
        for invalid in [
            "skill:0000000000000013:2/review",
            "skill:0000000000000013:0/../review",
            "skill:0000000000000013:0/..",
        ] {
            assert_eq!(
                locations.resolve(invalid),
                Err(LocationError::InvalidSkillLocation),
                "{invalid}"
            );
        }
    }

    #[test]
    fn skill_locations_decode_escaped_path_characters_without_permitting_traversal() {
        let locations = locations(1, &["/root"]);
        assert_eq!(
            locations.resolve("skill:0000000000000001:0/review%26more"),
            Ok(PathBuf::from("/root/review&more"))
        );
        for invalid in [
            "skill:0000000000000001:0/%2e%2e",
            "skill:0000000000000001:0/review%2fother",
            "skill:0000000000000001:0/review%5cother",
            "skill:0000000000000001:0/review%00",
            "skill:0000000000000001:0/review%ff",
            "skill:0000000000000001:0/review%2",
            "skill:0000000000000001:0/review%zz",
            "skill:0000000000000001:0/",
            "skill:0000000000000001",
            "skill:000000000000000g:0/review",
        ] {
            assert_eq!(
                locations.resolve(invalid),
                Err(LocationError::InvalidSkillLocation),
                "{invalid}"
            );
        }
        assert_eq!(
            locations.resolve("/installed/workflow"),
            Ok(PathBuf::from("/installed/workflow"))
        );
    }

    #[test]
    fn skill_locations_parse_numbers_the_way_upstream_does() {
        let locations = locations(0x13, &["/first", "/second"]);
        for (accepted, expected) in [
            ("skill:+000000000000013:1/review", "/second/review"),
            ("skill:0000_00000000013:+1/review", "/second/review"),
            ("skill:0000000000000013:0_1/review", "/second/review"),
            ("skill:0000000000000013:-0/review", "/first/review"),
        ] {
            assert_eq!(
                locations.resolve(accepted),
                Ok(PathBuf::from(expected)),
                "{accepted}"
            );
        }
        for rejected in [
            "skill:0000000000000013:-1/review",
            "skill:0000000000000013:_1/review",
            "skill:0000000000000013:1_/review",
            "skill:0000000000000013:/review",
            "skill:0000000000000013:18446744073709551616/review",
        ] {
            assert_eq!(
                locations.resolve(rejected),
                Err(LocationError::InvalidSkillLocation),
                "{rejected}"
            );
        }
    }
}
