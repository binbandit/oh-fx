const SEPARATOR: u8 = b'/';

pub fn basename(path: &[u8]) -> &[u8] {
    let Some(end) = path.iter().rposition(|byte| *byte != SEPARATOR) else {
        return &[];
    };
    let trimmed = &path[..=end];
    match trimmed.iter().rposition(|byte| *byte == SEPARATOR) {
        Some(slash) => &trimmed[slash + 1..],
        None => trimmed,
    }
}

pub fn dirname(path: &[u8]) -> Option<&[u8]> {
    let end = path.iter().rposition(|byte| *byte != SEPARATOR)?;
    let slash = path[..end].iter().rposition(|byte| *byte == SEPARATOR)?;
    if slash == 0 {
        return Some(&path[..1]);
    }
    Some(&path[..slash])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_ignores_trailing_separators() {
        assert_eq!(basename(b"src/main.zig"), b"main.zig");
        assert_eq!(basename(b"src/core/"), b"core");
        assert_eq!(basename(b"main.zig"), b"main.zig");
        assert_eq!(basename(b"/"), b"");
        assert_eq!(basename(b""), b"");
    }

    #[test]
    fn dirname_returns_parent_or_none() {
        assert_eq!(dirname(b"src/main.zig"), Some(&b"src"[..]));
        assert_eq!(dirname(b"src/core/**/*.zig"), Some(&b"src/core/**"[..]));
        assert_eq!(dirname(b"/main.zig"), Some(&b"/"[..]));
        assert_eq!(dirname(b"main.zig"), None);
        assert_eq!(dirname(b"/"), None);
        assert_eq!(dirname(b""), None);
    }
}
