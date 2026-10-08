#[cfg(test)]
mod tests;

use crate::command_lex::is_safe_env_assignment;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token<'a> {
    pub(crate) text: &'a str,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

fn command_hides_base_token(token: &str) -> bool {
    is_shell_command(token) || is_delegating_command(token) || is_privilege_prefix(token)
}

fn is_shell_command(token: &str) -> bool {
    matches!(
        token,
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "fish"
            | "cmd"
            | "powershell"
            | "pwsh"
    )
}

fn is_delegating_command(token: &str) -> bool {
    matches!(
        token,
        "env" | "xargs" | "nice" | "stdbuf" | "unbuffer" | "nohup" | "timeout" | "time"
    )
}

fn is_privilege_prefix(token: &str) -> bool {
    matches!(token, "sudo" | "doas" | "su")
}

pub(crate) fn base_command_token(command: &str) -> Option<&str> {
    if command.contains('\n') {
        return None;
    }
    let mut cursor = skip_whitespace(command, 0);
    while let Some(token) = next_token(command, cursor) {
        if is_env_assignment(token.text) {
            if !is_safe_env_assignment(token.text) {
                return None;
            }
            cursor = token.end;
            continue;
        }
        if command_hides_base_token(token.text) || !is_accepted_command_identifier(token.text) {
            return None;
        }
        return Some(token.text);
    }
    None
}

pub(crate) fn analysis_command_tail(command: &str) -> &str {
    let mut cursor = skip_whitespace(command, 0);
    let mut peeled = false;
    while let Some(token) = next_token(command, cursor) {
        cursor = match token.text {
            assignment if is_env_assignment(assignment) => {
                if !is_safe_env_assignment(assignment) {
                    return command;
                }
                token.end
            }
            "env" => match next_token(command, token.end) {
                Some(argument)
                    if is_env_assignment(argument.text)
                        && is_safe_env_assignment(argument.text) =>
                {
                    argument.end
                }
                _ => return command,
            },
            "nice" => skip_nice_args(command, token.end),
            "stdbuf" => skip_stdbuf_args(command, token.end),
            "unbuffer" | "nohup" | "time" => token.end,
            "timeout" => match next_token(command, token.end) {
                Some(duration) if is_timeout_duration(duration.text) => duration.end,
                _ => return command,
            },
            _ => return &command[token.start..],
        };
        peeled = true;
    }
    if peeled { &command[cursor..] } else { command }
}

pub(crate) fn skip_whitespace(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && is_whitespace(bytes[cursor]) {
        cursor += 1;
    }
    cursor
}

pub(crate) fn next_token(input: &str, start: usize) -> Option<Token<'_>> {
    let bytes = input.as_bytes();
    let token_start = skip_whitespace(input, start);
    if token_start >= bytes.len() {
        return None;
    }
    let mut token_end = token_start;
    while token_end < bytes.len() && !is_whitespace(bytes[token_end]) {
        token_end += 1;
    }
    Some(Token {
        text: &input[token_start..token_end],
        start: token_start,
        end: token_end,
    })
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

pub(crate) fn is_env_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(name, _)| is_env_name(name))
}

fn is_env_name(name: &str) -> bool {
    let Some((&first, rest)) = name.as_bytes().split_first() else {
        return false;
    };
    (first.is_ascii_uppercase() || first == b'_')
        && rest
            .iter()
            .all(|&byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_accepted_command_identifier(token: &str) -> bool {
    if token == "[" {
        return true;
    }
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut saw_letter = false;
    let mut prev_hyphen = false;
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'a'..=b'z' => {
                saw_letter = true;
                prev_hyphen = false;
            }
            b'0'..=b'9' => prev_hyphen = false,
            b'-' => {
                if index == 0 || index + 1 == bytes.len() || prev_hyphen {
                    return false;
                }
                prev_hyphen = true;
            }
            _ => return false,
        }
    }
    saw_letter && !prev_hyphen
}

fn skip_nice_args(command: &str, start: usize) -> usize {
    let Some(first) = next_token(command, start) else {
        return start;
    };
    if first.text == "-n" {
        return next_token(command, first.end).map_or(start, |value| value.end);
    }
    let bytes = first.text.as_bytes();
    if bytes.len() > 1 && bytes[0] == b'-' && bytes[1].is_ascii_digit() {
        return first.end;
    }
    start
}

fn skip_stdbuf_args(command: &str, start: usize) -> usize {
    let mut cursor = start;
    while let Some(token) = next_token(command, cursor) {
        if !token.text.starts_with('-') {
            break;
        }
        cursor = token.end;
    }
    cursor
}

fn is_timeout_duration(token: &str) -> bool {
    let bytes = token.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if byte == b'.' && !saw_dot && index + 1 < bytes.len() {
            saw_dot = true;
            continue;
        }
        if index + 1 == bytes.len() && matches!(byte, b's' | b'm' | b'h' | b'd') {
            return saw_digit;
        }
        return false;
    }
    saw_digit
}
