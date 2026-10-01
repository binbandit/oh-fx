use crate::command_lex::{ArgvToken, redirection_kind, tokenize_argv};

const MAX_COMMAND_BYTES: usize = 8 * 1024;
const DYNAMIC_SHELL_BYTES: [char; 16] = [
    '$', '`', '*', '?', '[', '~', '\n', '\r', ';', '(', ')', '{', '}', '^', '#', '!',
];
const INERT_FETCH_FLAGS: [&str; 24] = [
    "--all",
    "--append",
    "-a",
    "--atomic",
    "--dry-run",
    "--ipv4",
    "-4",
    "--ipv6",
    "-6",
    "--keep",
    "-k",
    "--multiple",
    "--no-recurse-submodules",
    "--no-tags",
    "-n",
    "--no-write-fetch-head",
    "--progress",
    "--no-progress",
    "--quiet",
    "-q",
    "--tags",
    "-t",
    "--verbose",
    "-v",
];
const INERT_FETCH_COUNTS: [&str; 3] = ["--depth=", "--deepen=", "--jobs="];
const INERT_PACKAGE_FLAGS: [&str; 30] = [
    "-D",
    "--save-dev",
    "-S",
    "--save",
    "-E",
    "--save-exact",
    "-O",
    "--save-optional",
    "-P",
    "--save-prod",
    "--save-peer",
    "--no-save",
    "--exact",
    "--dev",
    "--optional",
    "--peer",
    "--frozen-lockfile",
    "--immutable",
    "--prefer-offline",
    "--offline",
    "--no-audit",
    "--no-fund",
    "--legacy-peer-deps",
    "--ignore-scripts",
    "--no-optional",
    "--production",
    "--if-present",
    "--silent",
    "-s",
    "--verbose",
];
const INERT_ZIG_BUILD_FLAGS: [&str; 7] = [
    "--summary",
    "--verbose",
    "--color",
    "--release",
    "--prominent-compile-errors",
    "-freference-trace",
    "-fno-reference-trace",
];

pub fn known_reversible_auto_command(command: &str) -> bool {
    let trimmed = command.trim_matches([' ', '\t']);
    if trimmed.is_empty() || trimmed.len() > MAX_COMMAND_BYTES || trimmed.contains(['\n', '\r']) {
        return false;
    }
    let Ok(tokens) = tokenize_argv(trimmed) else {
        return false;
    };
    if tokens.is_empty() || tokens.iter().any(is_unsupported_auto_control_operator) {
        return false;
    }
    tokens.split(is_and_operator).all(reversible_auto_stage)
}

fn is_and_operator(token: &ArgvToken<'_>) -> bool {
    token.operator && token.value == "&&"
}

fn is_unsupported_auto_control_operator(token: &ArgvToken<'_>) -> bool {
    token.operator
        && matches!(
            token.value.as_str(),
            "|" | "||" | "&" | ";" | "(" | ")" | "{" | "}"
        )
}

fn is_unsupported_auto_operator(token: &ArgvToken<'_>) -> bool {
    is_unsupported_auto_control_operator(token)
        || (token.operator && redirection_kind(&token.value).is_some())
}

fn reversible_auto_stage(raw_tokens: &[ArgvToken<'_>]) -> bool {
    let tokens = strip_stderr_merge(raw_tokens);
    let Some((executable, words)) = tokens.split_first() else {
        return false;
    };
    if tokens
        .iter()
        .any(|token| is_unsupported_auto_operator(token) || has_dynamic_shell_syntax(token))
    {
        return false;
    }
    let words: Vec<&str> = words.iter().map(|token| token.value.as_str()).collect();
    match executable.value.as_str() {
        "node" => matches!(words.as_slice(), [flag] if is_version_flag(flag)),
        "which" => !words.is_empty() && all_operands(&words),
        "git" => reversible_git(&words),
        "npm" => reversible_package_command(&words, &["install", "i", "ci", "test"]),
        "bun" | "pnpm" | "yarn" => reversible_package_command(&words, &["install", "test"]),
        "zig" => {
            matches!(words.as_slice(), ["build", rest @ ..] if zig_build_arguments_are_inert(rest))
        }
        _ => false,
    }
}

fn has_dynamic_shell_syntax(token: &ArgvToken<'_>) -> bool {
    token.raw.contains(DYNAMIC_SHELL_BYTES) || token.raw.starts_with('=')
}

fn strip_stderr_merge<'a, 'b>(tokens: &'a [ArgvToken<'b>]) -> &'a [ArgvToken<'b>] {
    match tokens {
        [rest @ .., two, merge, one]
            if !two.operator
                && merge.operator
                && !one.operator
                && two.raw == "2"
                && merge.value == ">&"
                && one.raw == "1" =>
        {
            rest
        }
        _ => tokens,
    }
}

fn is_version_flag(value: &str) -> bool {
    value == "-v" || value == "--version"
}

fn all_operands(words: &[&str]) -> bool {
    words
        .iter()
        .all(|word| !word.is_empty() && !word.starts_with('-'))
}

fn reversible_git(words: &[&str]) -> bool {
    match words {
        ["status", ..] | ["remote", "-v"] | ["worktree", "list", ..] => true,
        ["fetch", rest @ ..] => rest.iter().all(|word| is_inert_fetch_word(word)),
        _ => false,
    }
}

fn is_inert_fetch_word(word: &str) -> bool {
    if word.starts_with('-') {
        return INERT_FETCH_FLAGS.contains(&word)
            || INERT_FETCH_COUNTS
                .iter()
                .any(|prefix| word.strip_prefix(prefix).is_some_and(is_decimal));
    }
    !word.is_empty() && !word.starts_with('+') && !word.contains("::")
}

fn reversible_package_command(words: &[&str], plain_subcommands: &[&str]) -> bool {
    match words {
        [flag] if is_version_flag(flag) => true,
        [subcommand, rest @ ..] if plain_subcommands.contains(subcommand) => {
            package_arguments_are_inert(rest)
        }
        ["run", script, rest @ ..] if !script.starts_with('-') => package_arguments_are_inert(rest),
        _ => false,
    }
}

fn package_arguments_are_inert(words: &[&str]) -> bool {
    options_before_separator(words)
        .iter()
        .all(|word| !word.starts_with('-') || INERT_PACKAGE_FLAGS.contains(word))
}

fn zig_build_arguments_are_inert(words: &[&str]) -> bool {
    options_before_separator(words).iter().all(|word| {
        !word.starts_with('-')
            || INERT_ZIG_BUILD_FLAGS.contains(word)
            || word.starts_with("-D")
            || word.starts_with("--release=")
            || word.strip_prefix("-j").is_some_and(is_decimal)
    })
}

fn options_before_separator<'a, 'b>(words: &'a [&'b str]) -> &'a [&'b str] {
    words
        .iter()
        .position(|word| *word == "--")
        .map_or(words, |separator| &words[..separator])
}

fn is_decimal(text: &str) -> bool {
    !text.is_empty() && text.len() <= 20 && text.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_reversible_auto_commands_exclude_destructive_and_hidden_effects() {
        for command in [
            "node -v",
            "node -v && npm -v",
            "which node npm && node -v",
            "git status --short --branch",
            "git remote -v",
            "git worktree list --porcelain",
            "git fetch origin main",
            "npm install 2>&1",
            "npm run dev",
            "zig build test",
            "bun test",
        ] {
            assert!(known_reversible_auto_command(command), "{command}");
        }
        for command in [
            "rm -rf .",
            "git rm -r netlify",
            "git push origin HEAD",
            "cat ~/.ssh/id_rsa",
            "sh -c 'npm install'",
            "npm exec -- rm -rf .",
            "npm install --prefix=/tmp/global",
            "npm install --location global",
            "git fetch --prune origin",
        ] {
            assert!(!known_reversible_auto_command(command), "{command}");
        }
    }

    #[test]
    fn reversible_commands_accept_every_plain_upstream_form() {
        for command in [
            "  git status\t",
            "git status && git remote -v && git worktree list",
            "git fetch",
            "git fetch --all --tags --depth=1 origin",
            "npm --version",
            "npm i",
            "npm ci",
            "npm test",
            "npm install --save-dev typescript",
            "npm run build -- --watch",
            "pnpm install --frozen-lockfile",
            "yarn run lint",
            "bun install 2>&1",
            "zig build -Doptimize=ReleaseFast -j8 test",
            "zig build run -- --anything",
            "which 'node'",
        ] {
            assert!(known_reversible_auto_command(command), "{command}");
        }
    }

    #[test]
    fn reversible_commands_reject_shell_syntax_and_redirections() {
        for command in [
            "",
            "   ",
            "git status\n",
            "git status\rrm -rf .",
            "git status; rm -rf .",
            "git status | sh",
            "git status || rm -rf .",
            "git status &",
            "git status > out.txt",
            "git status 2>&1 > out.txt",
            "git status 2> err.txt",
            "git status < in.txt",
            "npm test $(rm -rf .)",
            "npm test `rm -rf .`",
            "npm run 'a'$HOME",
            "npm test *",
            "git status ~",
            "(git status)",
            "{ git status; }",
            "FOO=1 npm test",
            "env npm test",
            "'git status'",
            "git \"status\"; ls",
            "git status 'unterminated",
            "git status \\",
            "node",
            "node -v extra",
            "which",
            "which -a node",
            "zig fmt .",
            "cargo test",
            "git fetch origin ^main",
            "git fetch origin main#1",
            "git fetch origin !main",
            "git fetch origin =git",
            "npm run '#build'",
        ] {
            assert!(!known_reversible_auto_command(command), "{command:?}");
        }
        assert!(!known_reversible_auto_command(&format!(
            "git status {}",
            "a".repeat(MAX_COMMAND_BYTES)
        )));
    }

    #[test]
    fn quoted_and_escaped_operators_stay_arguments_of_their_command() {
        for command in [
            "npm run review '&&' git status --script-shell=/not/a/shell",
            "npm run review \"&&\" git status --script-shell=/not/a/shell",
            "npm run review \\&\\& git status --script-shell=/not/a/shell",
            "npm install '&&' git status --prefix=/tmp/outside",
            "git fetch origin '&&' git fetch --upload-pack=evil origin",
        ] {
            assert!(!known_reversible_auto_command(command), "{command:?}");
        }
        for command in [
            "git status '&&' git",
            "git status \\&\\& git",
            "git status && git remote -v",
        ] {
            assert!(known_reversible_auto_command(command), "{command:?}");
        }
    }

    #[test]
    fn reversible_commands_reject_options_that_run_programs_or_rewrite_configuration() {
        for command in [
            "git fetch --upload-pack=touch\\ pwned origin",
            "git fetch --upload-pack touch origin",
            "git fetch --upload-p=evil origin",
            "git fetch -u evil origin",
            "git fetch --recurse-submodules=yes",
            "git fetch --set-upstream origin main",
            "git fetch --server-option=x origin",
            "git fetch -p origin",
            "git fetch --prune-tags origin",
            "git fetch --depth=abc origin",
            "git fetch origin +main:main",
            "git fetch ext::sh origin",
            "git fetch 'fd::17' origin",
            "git -c core.sshCommand=evil fetch",
            "npm install --script-shell=/bin/evil",
            "npm install --userconfig=./npmrc",
            "npm install --node-options=--require=./evil.js",
            "npm install -g left-pad",
            "npm install --global left-pad",
            "npm install --prefix /tmp/global",
            "npm run --script-shell=/bin/evil build",
            "npm run -s",
            "npm exec left-pad",
            "pnpm install --config.side-effects-cache=false",
            "yarn install --modules-folder /tmp",
            "bun install --cwd /tmp",
            "zig build --build-runner evil.zig",
            "zig build --build-file evil.zig",
            "zig build --zig-lib-dir /tmp",
            "zig build --prefix /usr",
            "zig build -fqemu",
            "zig build -jmany",
        ] {
            assert!(!known_reversible_auto_command(command), "{command:?}");
        }
    }
}
