const WHITESPACE: &[u8] = b" \t\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexError {
    EmbeddedNul,
    MalformedEscape,
    UnbalancedQuote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgvToken<'a> {
    pub raw: &'a str,
    pub value: String,
    pub operator: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedirectionKind {
    ReadPath,
    WritePath,
    ReadWritePath,
    Unsupported,
}

pub fn tokenize_argv(command: &str) -> Result<Vec<ArgvToken<'_>>, LexError> {
    if command.contains('\0') {
        return Err(LexError::EmbeddedNul);
    }
    let bytes = command.as_bytes();
    let mut tokens = Vec::new();
    let mut value = Vec::new();
    let mut token_start = None;
    let mut in_single = false;
    let mut in_double = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !in_single && !in_double {
            let operator_len = if is_whitespace(byte) {
                Some(0)
            } else {
                redirection_operator_len(bytes, index)
                    .or_else(|| control_operator_len(bytes, index))
            };
            if let Some(operator_len) = operator_len {
                if let Some(start) = token_start.take() {
                    tokens.push(word(command, start, index, &mut value));
                }
                if operator_len > 0 {
                    let operator = &command[index..index + operator_len];
                    tokens.push(ArgvToken {
                        raw: operator,
                        value: operator.to_owned(),
                        operator: true,
                    });
                }
                index += operator_len.max(1);
                continue;
            }
        }
        token_start.get_or_insert(index);
        match byte {
            b'\'' if !in_double => {
                in_single = !in_single;
                index += 1;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                index += 1;
            }
            b'\\' if !in_single => {
                let escaped = *bytes.get(index + 1).ok_or(LexError::MalformedEscape)?;
                value.push(escaped);
                index += 2;
            }
            _ => {
                value.push(byte);
                index += 1;
            }
        }
    }
    if in_single || in_double {
        return Err(LexError::UnbalancedQuote);
    }
    if let Some(start) = token_start {
        tokens.push(word(command, start, bytes.len(), &mut value));
    }
    Ok(tokens)
}

pub fn unsafe_compound_indicator(command: &str) -> bool {
    if command.contains('\0') {
        return true;
    }
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if !in_single => {
                if index + 1 >= bytes.len() {
                    return true;
                }
                escaped = true;
                continue;
            }
            b'\'' if !in_double => {
                in_single = !in_single;
                continue;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                continue;
            }
            _ if in_single || in_double => continue,
            _ => {}
        }
        let next = bytes.get(index + 1).copied();
        let unsafe_byte = match byte {
            b'$' => next == Some(b'('),
            b'`' | b'(' | b')' | b'{' | b'}' | b';' | b'\n' => true,
            b'&' => next != Some(b'>') && (index == 0 || bytes[index - 1] != b'>'),
            b'|' => next == Some(b'|'),
            _ => false,
        };
        if unsafe_byte {
            return true;
        }
    }
    in_single || in_double
}

fn word<'a>(command: &'a str, start: usize, end: usize, value: &mut Vec<u8>) -> ArgvToken<'a> {
    let decoded = String::from_utf8_lossy(value).into_owned();
    value.clear();
    ArgvToken {
        raw: &command[start..end],
        value: decoded,
        operator: false,
    }
}

fn redirection_operator_len(input: &[u8], start: usize) -> Option<usize> {
    let next = |offset: usize| input.get(start + offset).copied();
    match input[start] {
        b'&' if next(1) == Some(b'>') => Some(if next(2) == Some(b'>') { 3 } else { 2 }),
        b'<' if next(1) == Some(b'<') && next(2) == Some(b'<') => Some(3),
        b'<' if matches!(next(1), Some(b'<' | b'&' | b'>')) => Some(2),
        b'>' if matches!(next(1), Some(b'>' | b'|' | b'&')) => Some(2),
        b'<' | b'>' => Some(1),
        _ => None,
    }
}

pub(crate) fn redirection_kind(value: &str) -> Option<RedirectionKind> {
    match value {
        "<" => Some(RedirectionKind::ReadPath),
        "<>" => Some(RedirectionKind::ReadWritePath),
        "<<" | "<<<" | "<&" => Some(RedirectionKind::Unsupported),
        ">" | ">>" | ">|" | "&>" | "&>>" | ">&" => Some(RedirectionKind::WritePath),
        _ => None,
    }
}

fn control_operator_len(input: &[u8], start: usize) -> Option<usize> {
    let doubled = |byte: u8| input.get(start + 1) == Some(&byte);
    match input[start] {
        b';' | b'(' | b')' | b'{' | b'}' => Some(1),
        b'|' => Some(if doubled(b'|') { 2 } else { 1 }),
        b'&' => Some(if doubled(b'&') { 2 } else { 1 }),
        _ => None,
    }
}

pub(crate) fn is_safe_env_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(name, _)| is_safe_env_name(name))
}

fn is_safe_env_name(name: &str) -> bool {
    matches!(
        name,
        "CI" | "NODE_ENV"
            | "BUN_ENV"
            | "DENO_ENV"
            | "PYTHONUNBUFFERED"
            | "PYTHONWARNINGS"
            | "RUST_LOG"
            | "RUST_BACKTRACE"
            | "TERM"
            | "COLORTERM"
            | "NO_COLOR"
            | "FORCE_COLOR"
            | "CLICOLOR"
            | "CLICOLOR_FORCE"
            | "LANG"
            | "LC_ALL"
            | "LC_CTYPE"
            | "LC_MESSAGES"
            | "TZ"
            | "CC"
            | "CXX"
            | "AR"
            | "AS"
            | "LD"
            | "CFLAGS"
            | "CXXFLAGS"
            | "CPPFLAGS"
            | "LDFLAGS"
            | "MAKEFLAGS"
            | "CMAKE_BUILD_TYPE"
    )
}

fn is_whitespace(byte: u8) -> bool {
    WHITESPACE.contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(command: &str) -> Vec<String> {
        tokenize_argv(command)
            .unwrap()
            .into_iter()
            .map(|token| token.value)
            .collect()
    }

    #[test]
    fn tokenize_argv_decodes_single_and_double_quoted_tokens() {
        let tokens = tokenize_argv("grep 'hello world' \"file name.txt\"").unwrap();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].value, "hello world");
        assert_eq!(tokens[2].value, "file name.txt");
        assert!(tokens.iter().all(|token| !token.operator));
    }

    #[test]
    fn tokenize_argv_preserves_raw_slices() {
        let tokens = tokenize_argv("  cat 'a b'").unwrap();
        assert_eq!(tokens[0].raw, "cat");
        assert_eq!(tokens[1].raw, "'a b'");
    }

    #[test]
    fn tokenize_argv_handles_escaped_spaces_without_splitting() {
        let tokens = tokenize_argv("cat file\\ name.txt").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].value, "file name.txt");
        assert_eq!(tokens[1].raw, "file\\ name.txt");
    }

    #[test]
    fn tokenize_argv_emits_redirection_operators_as_tokens() {
        let tokens = values(
            "printf ok>out.txt &>> log.txt >&err.txt < in.txt <> state.txt << EOF <<< value <& 0",
        );
        assert_eq!(tokens[2], ">");
        assert_eq!(tokens[4], "&>>");
        assert_eq!(tokens[6], ">&");
        assert_eq!(tokens[7], "err.txt");
        assert_eq!(tokens[8], "<");
        assert_eq!(tokens[10], "<>");
        assert_eq!(tokens[12], "<<");
        assert_eq!(tokens[14], "<<<");
        assert_eq!(tokens[16], "<&");
    }

    #[test]
    fn tokenize_argv_splits_control_operators_outside_quotes() {
        assert_eq!(
            values("a&&b||c;d|e&f(g){h}"),
            [
                "a", "&&", "b", "||", "c", ";", "d", "|", "e", "&", "f", "(", "g", ")", "{", "h",
                "}"
            ]
        );
        assert_eq!(values("2>&1"), ["2", ">&", "1"]);
        assert_eq!(values("'a;b' \"c|d\""), ["a;b", "c|d"]);
        let operators = tokenize_argv("a && b").unwrap();
        assert!(operators[1].operator);
        for literal in ["'&&'", "\"&&\"", "\\&\\&", "'>'", "\\|"] {
            let tokens = tokenize_argv(literal).unwrap();
            assert_eq!(tokens.len(), 1, "{literal}");
            assert!(!tokens[0].operator, "{literal}");
        }
    }

    #[test]
    fn tokenize_argv_unescapes_inside_double_quotes_but_not_single_quotes() {
        assert_eq!(values(r#""a\"b" 'c\d'"#), ["a\"b", "c\\d"]);
    }

    #[test]
    fn redirection_kind_classifies_shell_redirects() {
        assert_eq!(redirection_kind("<"), Some(RedirectionKind::ReadPath));
        assert_eq!(redirection_kind("<>"), Some(RedirectionKind::ReadWritePath));
        assert_eq!(redirection_kind(">"), Some(RedirectionKind::WritePath));
        assert_eq!(redirection_kind("&>>"), Some(RedirectionKind::WritePath));
        assert_eq!(redirection_kind("<<"), Some(RedirectionKind::Unsupported));
        assert_eq!(redirection_kind("<<<"), Some(RedirectionKind::Unsupported));
        assert_eq!(redirection_kind("<&"), Some(RedirectionKind::Unsupported));
        assert_eq!(redirection_kind("|"), None);
    }

    #[test]
    fn tokenize_argv_distinguishes_empty_argv_from_tokenization_failure() {
        assert!(tokenize_argv("   \t ").unwrap().is_empty());
        assert_eq!(
            tokenize_argv("\"unterminated"),
            Err(LexError::UnbalancedQuote)
        );
    }

    #[test]
    fn tokenize_argv_reports_malformed_trailing_escapes() {
        assert_eq!(tokenize_argv("printf x\\"), Err(LexError::MalformedEscape));
    }

    #[test]
    fn unsafe_compound_indicator_detects_subshells_groups_and_logical_control() {
        for command in [
            "echo $(pwd)",
            "(cd src; make)",
            "{ echo hi; }",
            "cd src && touch x",
            "grep x file || true",
            "sleep 1 &",
            "echo `id`",
            "printf a\nprintf b",
            "printf 'open",
            "printf x\\",
            "printf \0",
        ] {
            assert!(unsafe_compound_indicator(command), "{command:?}");
        }
    }

    #[test]
    fn unsafe_compound_indicator_ignores_pipes_redirects_and_quoted_or_escaped_syntax() {
        for command in [
            "printf a | grep a",
            "printf \\& \\| \\;",
            "printf \"$(literal)\"",
            "printf ok &> log.txt",
            "printf ok >& log.txt",
            "git push origin 'a;b'",
        ] {
            assert!(!unsafe_compound_indicator(command), "{command:?}");
        }
    }

    #[test]
    fn tokenize_argv_rejects_embedded_nul() {
        assert_eq!(tokenize_argv("cat \0 file"), Err(LexError::EmbeddedNul));
    }
}
