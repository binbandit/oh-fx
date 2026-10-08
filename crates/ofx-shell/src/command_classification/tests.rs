use super::{analysis_command_tail, base_command_token, command_hides_base_token};

#[test]
fn literal_pattern_predicate_covers_delegated_and_privileged_commands() {
    for token in ["bash", "cmd", "env", "nice", "sudo", "doas", "su"] {
        assert!(command_hides_base_token(token), "{token}");
    }
    assert!(!command_hides_base_token("grep"));
}

#[test]
fn analysis_command_tail_leaves_bare_command_unchanged() {
    assert_eq!(
        analysis_command_tail("grep foo bar.txt"),
        "grep foo bar.txt"
    );
}

#[test]
fn analysis_command_tail_starts_at_first_plain_command_token() {
    assert_eq!(analysis_command_tail("nice grep foo"), "grep foo");
}

#[test]
fn known_env_prefix_is_transparent_to_command_analysis() {
    assert_eq!(base_command_token("NODE_ENV=prod grep foo"), Some("grep"));
    assert_eq!(analysis_command_tail("NODE_ENV=prod grep foo"), "grep foo");
}

#[test]
fn unknown_env_prefix_fails_closed() {
    assert_eq!(base_command_token("MY_SECRET=x grep foo"), None);
    assert_eq!(
        analysis_command_tail("MY_SECRET=x grep foo"),
        "MY_SECRET=x grep foo"
    );
}

#[test]
fn shell_first_word_is_not_analyzed_as_base_command() {
    assert_eq!(base_command_token("bash -c 'echo hi'"), None);
    assert_eq!(
        analysis_command_tail("bash -c 'echo hi'"),
        "bash -c 'echo hi'"
    );
}

#[test]
fn priority_adjustment_prefix_is_transparent_to_policy_analysis() {
    assert_eq!(analysis_command_tail("nice grep foo"), "grep foo");
}

#[test]
fn timeout_prefix_advances_past_its_duration_argument() {
    assert_eq!(
        analysis_command_tail("timeout 5 grep foo bar"),
        "grep foo bar"
    );
}

#[test]
fn env_command_is_transparent_only_with_a_known_assignment() {
    assert_eq!(
        analysis_command_tail("env NODE_ENV=prod grep foo"),
        "grep foo"
    );
}

#[test]
fn env_command_with_flag_fails_closed() {
    assert_eq!(analysis_command_tail("env -i grep foo"), "env -i grep foo");
}

#[test]
fn env_command_with_unknown_assignment_fails_closed() {
    assert_eq!(
        analysis_command_tail("env MY_SECRET=x grep foo"),
        "env MY_SECRET=x grep foo"
    );
}

#[test]
fn env_command_with_bare_command_fails_closed() {
    assert_eq!(analysis_command_tail("env grep foo"), "env grep foo");
}

#[test]
fn xargs_remains_the_analysis_command_because_stdin_supplies_arguments() {
    assert_eq!(analysis_command_tail("xargs grep foo"), "xargs grep foo");
}

#[test]
fn privilege_prefixes_remain_anchored_during_generic_analysis() {
    assert_eq!(analysis_command_tail("sudo rm -rf /"), "sudo rm -rf /");
    assert_eq!(analysis_command_tail("doas rm -rf /"), "doas rm -rf /");
    assert_eq!(
        analysis_command_tail("su -c 'rm -rf /'"),
        "su -c 'rm -rf /'"
    );
}

#[test]
fn base_command_token_returns_normal_command_identifiers() {
    assert_eq!(base_command_token("python3 file.py"), Some("python3"));
}

#[test]
fn base_command_token_rejects_unknown_env_prefix() {
    assert_eq!(base_command_token("MY_SECRET=x grep foo"), None);
}

#[test]
fn base_command_token_rejects_unsafe_token_shapes() {
    for command in ["-rf /tmp", "./script", "123", "foo.bar"] {
        assert_eq!(base_command_token(command), None, "{command}");
    }
}

#[test]
fn base_command_token_accepts_bracket_alias_only_as_symbolic_token() {
    assert_eq!(base_command_token("[ -f foo ]"), Some("["));
    assert_eq!(base_command_token("] -f foo"), None);
}

#[test]
fn empty_input_returns_null_or_original_slice() {
    assert_eq!(base_command_token(" \t "), None);
    assert_eq!(analysis_command_tail(" \t "), " \t ");
}

#[test]
fn multiline_first_word_fails_closed_while_analysis_stays_anchored() {
    assert_eq!(base_command_token("grep foo\nrm bar"), None);
    assert_eq!(
        analysis_command_tail("grep foo\nrm bar"),
        "grep foo\nrm bar"
    );
}
