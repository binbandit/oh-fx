use crate::fs_path;

pub const MAX_PATTERN_BYTES: usize = 4096;

const SEPARATOR: u8 = b'/';
const DOUBLE_STAR: &[u8] = b"**";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error("PatternTooLong")]
    PatternTooLong,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    raw: Vec<u8>,
    segments: Option<Vec<Vec<u8>>>,
}

impl Pattern {
    pub fn compile(raw: &[u8]) -> Result<Self, CompileError> {
        Self::compile_with_segments(raw, raw.contains(&SEPARATOR))
    }

    pub fn compile_anchored(raw: &[u8]) -> Result<Self, CompileError> {
        Self::compile_with_segments(raw, true)
    }

    fn compile_with_segments(raw: &[u8], segmented: bool) -> Result<Self, CompileError> {
        if raw.len() > MAX_PATTERN_BYTES {
            return Err(CompileError::PatternTooLong);
        }
        Ok(Self {
            raw: raw.to_vec(),
            segments: segmented.then(|| split_segments(raw)),
        })
    }

    pub(crate) fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn matches_path(&self, candidate_path: &[u8]) -> bool {
        match &self.segments {
            None => match_segment(&self.raw, fs_path::basename(candidate_path)),
            Some(segments) => match_segmented(segments, candidate_path),
        }
    }

    pub(crate) fn matches_basename(&self, basename: &[u8]) -> bool {
        self.segments.is_none() && match_segment(&self.raw, basename)
    }
}

fn split_segments(path: &[u8]) -> Vec<Vec<u8>> {
    path_segments(path).map(<[u8]>::to_vec).collect()
}

fn path_segments(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut segments = path.split(|byte| *byte == SEPARATOR).peekable();
    std::iter::from_fn(move || {
        let segment = segments.next()?;
        let is_last = segments.peek().is_none();
        (!(is_last && segment.is_empty())).then_some(segment)
    })
}

fn match_segmented(pattern_segments: &[Vec<u8>], candidate_path: &[u8]) -> bool {
    let segment_count = pattern_segments.len();
    let mut previous = vec![false; segment_count + 1];
    let mut current = vec![false; segment_count + 1];

    previous[0] = true;
    for (index, segment) in pattern_segments.iter().enumerate() {
        previous[index + 1] = previous[index] && segment == DOUBLE_STAR;
    }

    for candidate_segment in path_segments(candidate_path) {
        current.fill(false);
        for (index, pattern_segment) in pattern_segments.iter().enumerate() {
            current[index + 1] = if pattern_segment == DOUBLE_STAR {
                current[index] || previous[index + 1]
            } else {
                previous[index] && match_segment(pattern_segment, candidate_segment)
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[segment_count]
}

fn match_segment(pattern: &[u8], candidate: &[u8]) -> bool {
    let mut pattern_index = 0;
    let mut candidate_index = 0;
    let mut star_pattern_index: Option<usize> = None;
    let mut star_candidate_index = 0;

    while candidate_index < candidate.len() {
        if pattern_index < pattern.len() {
            let token = pattern[pattern_index];
            if token == b'*' {
                while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
                    pattern_index += 1;
                }
                if pattern_index == pattern.len() {
                    return true;
                }
                star_pattern_index = Some(pattern_index);
                star_candidate_index = candidate_index;
                continue;
            }
            if token == b'?' || token == candidate[candidate_index] {
                if candidate[candidate_index] == SEPARATOR {
                    return false;
                }
                pattern_index += 1;
                candidate_index += 1;
                continue;
            }
        }

        let Some(retry_pattern_index) = star_pattern_index else {
            return false;
        };
        star_candidate_index += 1;
        candidate_index = star_candidate_index;
        pattern_index = retry_pattern_index;
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_compiled_match(pattern: &str, candidate_path: &str, expected: bool) {
        let compiled = Pattern::compile(pattern.as_bytes()).unwrap();
        assert_eq!(
            compiled.matches_path(candidate_path.as_bytes()),
            expected,
            "{pattern} against {candidate_path}"
        );
    }

    #[test]
    fn glob_pattern_matches_recursive_and_segment_wildcards() {
        expect_compiled_match("**/*.zig", "main.zig", true);
        expect_compiled_match("**/*.zig", "src/core/main.zig", true);
        expect_compiled_match("*.zig", "src/main.zig", true);
        expect_compiled_match("file?.zig", "dir/file1.zig", true);
        expect_compiled_match("file?.zig", "dir/file12.zig", false);
        expect_compiled_match("foo\\bar.txt", "src/foo\\bar.txt", true);
        expect_compiled_match("src/*.zig", "src/main.zig", true);
        expect_compiled_match("src/*.zig", "src/core/main.zig", false);
        expect_compiled_match("src/*.zig", "lib/src/main.zig", false);
    }

    #[test]
    fn glob_pattern_preserves_globstar_zero_or_more_segment_semantics() {
        expect_compiled_match("**", "", true);
        expect_compiled_match("src/", "src", true);
        expect_compiled_match("src/**", "src", true);
        expect_compiled_match("src/**", "src/core/main.zig", true);
        expect_compiled_match("src/**/main.zig", "src/main.zig", true);
        expect_compiled_match("src/**/main.zig", "src/core/nested/main.zig", true);
        expect_compiled_match("src/**/main.zig", "lib/src/core/main.zig", false);
    }

    #[test]
    fn glob_pattern_treats_backslashes_literally() {
        expect_compiled_match("dir\\*.zig", "dir\\main.zig", true);
        expect_compiled_match("dir\\*.zig", "dir/main.zig", false);
    }

    #[test]
    fn glob_pattern_handles_adversarial_repeated_star_nonmatches_without_recursion() {
        let star_count = 1024;
        let mut consecutive = vec![b'*'; star_count];
        consecutive.push(b'z');
        let candidate = vec![b'a'; star_count];

        let compiled_consecutive = Pattern::compile(&consecutive).unwrap();
        assert!(!compiled_consecutive.matches_path(&candidate));

        let mut separated = b"*a".repeat(512);
        separated.push(b'z');
        let compiled_separated = Pattern::compile(&separated).unwrap();
        assert!(!compiled_separated.matches_path(&candidate));
    }

    #[test]
    fn glob_pattern_rejects_patterns_beyond_the_maximum_accepted_length() {
        let accepted = vec![b'a'; MAX_PATTERN_BYTES];
        let accepted_pattern = Pattern::compile(&accepted).unwrap();
        assert!(accepted_pattern.matches_path(&accepted));

        let rejected = vec![b'a'; MAX_PATTERN_BYTES + 1];
        assert_eq!(
            Pattern::compile(&rejected),
            Err(CompileError::PatternTooLong)
        );
    }

    #[test]
    fn anchored_patterns_match_whole_relative_paths_even_without_a_separator() {
        let anchored = Pattern::compile_anchored(b"*.rs").unwrap();
        assert!(anchored.matches_path(b"direct.rs"));
        assert!(!anchored.matches_path(b"nested/other.rs"));
        assert!(!anchored.matches_basename(b"direct.rs"));
        let recursive = Pattern::compile_anchored(b"**/*.rs").unwrap();
        assert!(recursive.matches_path(b"direct.rs"));
        assert!(recursive.matches_path(b"nested/other.rs"));
        assert!(
            Pattern::compile(b"*.rs")
                .unwrap()
                .matches_path(b"nested/other.rs")
        );
    }

    #[test]
    fn glob_pattern_treats_brackets_and_braces_literally() {
        expect_compiled_match("*.{md,txt}", "notes.md", false);
        expect_compiled_match("[ab].txt", "a.txt", false);
        expect_compiled_match("[ab].txt", "dir/[ab].txt", true);
    }
}
