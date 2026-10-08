#[cfg(test)]
mod tests;

use crate::command_classification::{
    Token, analysis_command_tail, base_command_token, is_env_assignment, next_token,
    skip_whitespace,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestructiveEffect {
    DiscardVersionControlState,
    RemoveFiles,
}

#[derive(Default)]
struct GitEffectArgs {
    dry_run: bool,
    hard_reset: bool,
    has_operand: bool,
}

pub(crate) fn destructive_effect(command: &str) -> Option<DestructiveEffect> {
    destructive_effect_in_analysis(analysis_command_tail(command))
}

pub fn command_risk_note(command: &str) -> Option<&'static str> {
    destructive_effect(command).map(risk_note_for)
}

pub fn command_safer_alternative(command: &str) -> Option<&'static str> {
    let analysis = analysis_command_tail(command);
    if let Some(risk) = destructive_effect_in_analysis(analysis) {
        return Some(safer_alternative_for_risk(risk));
    }
    if has_shell_boundary(command) {
        return None;
    }
    match base_command_token(analysis)? {
        "cat" | "less" | "more" => Some("safer: use read_file for file inspection"),
        "ls" | "find" => Some("safer: use glob_files for discovery"),
        base if is_pattern_matcher(base) => Some("safer: use grep_files for exact local search"),
        _ => None,
    }
}

fn risk_note_for(risk: DestructiveEffect) -> &'static str {
    match risk {
        DestructiveEffect::DiscardVersionControlState => {
            "note: command may discard version-control state"
        }
        DestructiveEffect::RemoveFiles => "note: command may remove files forcefully",
    }
}

fn safer_alternative_for_risk(risk: DestructiveEffect) -> &'static str {
    match risk {
        DestructiveEffect::DiscardVersionControlState => {
            "safer: inspect git status first and revert only the intended files"
        }
        DestructiveEffect::RemoveFiles => "safer: inspect targets first",
    }
}

fn destructive_effect_in_analysis(command: &str) -> Option<DestructiveEffect> {
    let bytes = command.as_bytes();
    let mut effect = None;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut in_comment = false;
    let mut word_active = false;
    let mut segment_start = 0;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let at = index;
        index += 1;
        if in_comment {
            if byte == b'\n' {
                in_comment = false;
                word_active = false;
                segment_start = index;
            }
            continue;
        }
        if escaped {
            escaped = false;
            if byte != b'\n' {
                word_active = true;
            }
            continue;
        }
        match byte {
            b'\\' if !in_single => {
                escaped = true;
                continue;
            }
            b'\'' if !in_double => {
                in_single = !in_single;
                word_active = true;
                continue;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                word_active = true;
                continue;
            }
            b'$' | b'`' if !in_single => return None,
            _ if in_single || in_double => continue,
            b'#' if !word_active => {
                effect =
                    effect.or_else(|| warning_at_command_position(&command[segment_start..at]));
                in_comment = true;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                word_active = false;
                continue;
            }
            b'<' | b'>' | b'(' | b')' | b'{' | b'}' => return None,
            b';' | b'\n' => {}
            b'&' | b'|' => {
                if bytes.get(index) == Some(&byte) {
                    index += 1;
                }
            }
            _ => {
                word_active = true;
                continue;
            }
        }
        effect = effect.or_else(|| warning_at_command_position(&command[segment_start..at]));
        word_active = false;
        segment_start = index;
    }
    if in_single || in_double || escaped {
        return None;
    }
    if !in_comment {
        effect = effect.or_else(|| warning_at_command_position(&command[segment_start..]));
    }
    effect
}

fn warning_at_command_position(mut command: &str) -> Option<DestructiveEffect> {
    loop {
        let mut cursor = skip_whitespace(command, 0);
        while let Some(token) = next_token(command, cursor) {
            if !is_env_assignment(token.text) {
                break;
            }
            cursor = token.end;
        }
        let first = next_token(command, cursor)?;
        if let Some(tail) = privileged_command_tail(command, first) {
            command = tail;
            continue;
        }
        let rest = &command[first.end..];
        return git_destructive_effect(first.text, rest)
            .or_else(|| file_removal_effect(first.text, rest));
    }
}

fn privileged_command_tail<'a>(command: &'a str, first: Token<'_>) -> Option<&'a str> {
    match first.text {
        "sudo" | "doas" => sudo_like_command_tail(command, first.end),
        "su" => su_command_tail(command, first.end),
        _ => None,
    }
}

fn sudo_like_command_tail(command: &str, start: usize) -> Option<&str> {
    let mut cursor = start;
    while let Some(token) = next_token(command, cursor) {
        if token.text == "--" {
            let next = next_token(command, token.end)?;
            return Some(&command[next.start..]);
        }
        if is_env_assignment(token.text) {
            cursor = token.end;
            continue;
        }
        if token.text.starts_with('-') {
            cursor = if privilege_option_takes_value(token.text) {
                next_token(command, token.end)?.end
            } else {
                token.end
            };
            continue;
        }
        return Some(&command[token.start..]);
    }
    None
}

fn privilege_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "-u" | "-g"
            | "-h"
            | "-p"
            | "-C"
            | "-T"
            | "-U"
            | "--user"
            | "--group"
            | "--host"
            | "--prompt"
            | "--close-from"
            | "--command-timeout"
            | "--other-user"
    )
}

fn su_command_tail(command: &str, start: usize) -> Option<&str> {
    let mut cursor = start;
    while let Some(token) = next_token(command, cursor) {
        if token.text == "-c" {
            return parse_su_command_argument(command, token.end);
        }
        cursor = token.end;
    }
    None
}

fn parse_su_command_argument(command: &str, start: usize) -> Option<&str> {
    let cursor = skip_whitespace(command, start);
    let bytes = command.as_bytes();
    let &quote = bytes.get(cursor)?;
    if quote == b'\'' || quote == b'"' {
        let payload_start = cursor + 1;
        let mut escaped = false;
        for (offset, &byte) in bytes[payload_start..].iter().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            if quote == b'"' && byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == quote {
                return Some(&command[payload_start..payload_start + offset]);
            }
        }
        return None;
    }
    next_token(command, cursor).map(|token| token.text)
}

fn git_destructive_effect(command_name: &str, rest: &str) -> Option<DestructiveEffect> {
    if basename(command_name) != "git" {
        return None;
    }
    let sub = git_subcommand(rest)?;
    let args = git_effect_args(sub.text, &rest[sub.end..])?;
    match sub.text {
        "reset" if args.hard_reset => Some(DestructiveEffect::DiscardVersionControlState),
        "clean" if !args.dry_run => Some(DestructiveEffect::RemoveFiles),
        "rm" if !args.dry_run && args.has_operand => Some(DestructiveEffect::RemoveFiles),
        _ => None,
    }
}

fn git_effect_args(subcommand: &str, rest: &str) -> Option<GitEffectArgs> {
    let clean = subcommand == "clean";
    let mut args = GitEffectArgs::default();
    let mut help_or_version = false;
    let mut options = true;
    let mut offset = 0;
    while let Some(token) = next_token(rest, offset) {
        offset = token.end;
        let text = token.text;
        if token_starts_shell_comment(text) {
            break;
        }
        if options && text == "--" {
            options = false;
            continue;
        }
        if !options {
            args.has_operand = true;
            continue;
        }
        if clean {
            if let Some(exclude_index) = clean_exclude_short_index(text) {
                let flags = &text.as_bytes()[1..exclude_index];
                if flags.contains(&b'n') {
                    args.dry_run = true;
                }
                if flags.contains(&b'h') {
                    help_or_version = true;
                }
                if exclude_index + 1 == text.len() {
                    offset = exclude_pattern_end(rest, offset)?;
                }
                continue;
            }
            if text == "--exclude" {
                offset = exclude_pattern_end(rest, offset)?;
                continue;
            }
            if let Some(pattern) = text.strip_prefix("--exclude=") {
                if pattern.is_empty() {
                    return None;
                }
                continue;
            }
        }
        if matches!(text, "--help" | "--version" | "-h") || short_option_contains(text, b'h') {
            help_or_version = true;
        }
        if text == "--hard" {
            args.hard_reset = true;
        }
        if matches!(text, "--dry-run" | "-n") || short_option_contains(text, b'n') {
            args.dry_run = true;
        }
        if !clean && text == "--pathspec-from-file" {
            let Some(path) = next_token(rest, offset) else {
                break;
            };
            if token_starts_shell_comment(path.text) {
                break;
            }
            args.has_operand = true;
            offset = path.end;
            continue;
        }
        if !clean
            && text
                .strip_prefix("--pathspec-from-file=")
                .is_some_and(|path| !path.is_empty())
        {
            args.has_operand = true;
            continue;
        }
        if !text.starts_with('-') {
            args.has_operand = true;
        }
    }
    (!help_or_version).then_some(args)
}

fn exclude_pattern_end(rest: &str, offset: usize) -> Option<usize> {
    next_token(rest, offset)
        .filter(|pattern| !token_starts_shell_comment(pattern.text))
        .map(|pattern| pattern.end)
}

fn clean_exclude_short_index(option: &str) -> Option<usize> {
    let bytes = option.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'-' || bytes[1] == b'-' {
        return None;
    }
    bytes[1..]
        .iter()
        .position(|&byte| byte == b'e')
        .map(|relative| relative + 1)
}

fn short_option_contains(option: &str, needle: u8) -> bool {
    let bytes = option.as_bytes();
    bytes.len() > 2 && bytes[0] == b'-' && bytes[1] != b'-' && bytes[1..].contains(&needle)
}

fn git_subcommand(rest: &str) -> Option<Token<'_>> {
    let mut offset = 0;
    while let Some(token) = next_token(rest, offset) {
        if token_starts_shell_comment(token.text) {
            return None;
        }
        if token.text == "--" {
            let subcommand = next_token(rest, token.end)?;
            return (!token_starts_shell_comment(subcommand.text)).then_some(subcommand);
        }
        if !token.text.starts_with('-') {
            return Some(token);
        }
        if git_global_option_takes_value(token.text) {
            let value = next_token(rest, token.end)?;
            if token_starts_shell_comment(value.text) {
                return None;
            }
            offset = value.end;
            continue;
        }
        if git_global_option_has_inline_value(token.text) || git_global_flag(token.text) {
            offset = token.end;
            continue;
        }
        return None;
    }
    None
}

fn git_global_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
    )
}

fn git_global_option_has_inline_value(option: &str) -> bool {
    if (option.starts_with("-C") || option.starts_with("-c")) && option.len() > 2 {
        return true;
    }
    [
        "--git-dir=",
        "--work-tree=",
        "--namespace=",
        "--config-env=",
    ]
    .iter()
    .any(|prefix| {
        option
            .strip_prefix(prefix)
            .is_some_and(|value| !value.is_empty())
    })
}

fn git_global_flag(option: &str) -> bool {
    matches!(
        option,
        "--bare"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks"
            | "--no-pager"
            | "--paginate"
            | "-p"
            | "-P"
    )
}

fn file_removal_effect(command_name: &str, rest: &str) -> Option<DestructiveEffect> {
    let executable = basename(command_name);
    if !matches!(executable, "rm" | "rmdir" | "unlink" | "shred") {
        return None;
    }
    removal_has_operand(executable, rest).then_some(DestructiveEffect::RemoveFiles)
}

fn removal_has_operand(executable: &str, rest: &str) -> bool {
    let mut offset = 0;
    while let Some(token) = next_token(rest, offset) {
        offset = token.end;
        let text = token.text;
        if token_starts_shell_comment(text) || matches!(text, "--help" | "--version") {
            return false;
        }
        if text == "--" {
            return next_token(rest, offset)
                .is_some_and(|operand| !token_starts_shell_comment(operand.text));
        }
        if executable == "shred" && shred_option_takes_value(text) {
            let Some(value) = next_token(rest, offset) else {
                return false;
            };
            if token_starts_shell_comment(value.text) {
                return false;
            }
            offset = value.end;
            continue;
        }
        if text == "-" || !text.starts_with('-') {
            return true;
        }
    }
    false
}

fn shred_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "-n" | "-s" | "--iterations" | "--size" | "--random-source"
    )
}

fn token_starts_shell_comment(token: &str) -> bool {
    token.starts_with('#')
}

fn is_pattern_matcher(command: &str) -> bool {
    matches!(command, "grep" | "egrep" | "fgrep" | "rg" | "ag")
}

fn has_shell_boundary(command: &str) -> bool {
    command.contains([';', '&', '|', '<', '>', '$', '`', '\n'])
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rfind('/')
        .map_or(trimmed, |slash| &trimmed[slash + 1..])
}
