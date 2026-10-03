use super::{DestructiveEffect, command_risk_note, command_safer_alternative, destructive_effect};

#[test]
fn command_risk_note_detects_git_hard_reset() {
    assert_eq!(
        command_risk_note("git reset --hard"),
        Some("note: command may discard version-control state")
    );
}

#[test]
fn command_safer_alternative_explains_risky_commands() {
    assert_eq!(
        command_safer_alternative("git reset --hard"),
        Some("safer: inspect git status first and revert only the intended files")
    );
    assert_eq!(
        command_safer_alternative("rm -rf /tmp/x"),
        Some("safer: inspect targets first")
    );
}

#[test]
fn command_safer_alternative_maps_shell_inspection_to_dedicated_tools() {
    assert_eq!(
        command_safer_alternative("cat src/main.zig"),
        Some("safer: use read_file for file inspection")
    );
    assert_eq!(
        command_safer_alternative("ls src"),
        Some("safer: use glob_files for discovery")
    );
    assert_eq!(
        command_safer_alternative("rg needle src"),
        Some("safer: use grep_files for exact local search")
    );
    assert_eq!(
        command_safer_alternative("find src -name '*.zig'"),
        Some("safer: use glob_files for discovery")
    );
}

#[test]
fn command_safer_alternative_stays_conservative_for_compound_commands() {
    assert_eq!(command_safer_alternative("cat src/main.zig | wc -l"), None);
    assert_eq!(command_safer_alternative("git status"), None);
}

#[test]
fn command_risk_note_stays_narrow_for_ordinary_git_operations() {
    for command in [
        "git status",
        "git log --oneline",
        "git commit --no-verify -m ok",
    ] {
        assert_eq!(command_risk_note(command), None, "{command}");
    }
}

#[test]
fn command_risk_note_detects_forceful_removal() {
    assert_eq!(
        command_risk_note("rm -rf /tmp/x"),
        Some("note: command may remove files forcefully")
    );
}

#[test]
fn command_risk_note_detects_direct_destructive_effects_only() {
    for command in [
        "rm scratch.txt",
        "rmdir generated",
        "unlink stale-link",
        "shred secret.txt",
        "git clean -fd",
        "git rm tracked.txt",
        "/bin/rm scratch.txt",
        "/usr/bin/git clean -fd",
        "git -C nested clean -fd",
        "git --git-dir=.git rm tracked.txt",
    ] {
        assert_eq!(
            destructive_effect(command),
            Some(DestructiveEffect::RemoveFiles),
            "{command}"
        );
        assert_eq!(
            command_risk_note(command),
            Some("note: command may remove files forcefully"),
            "{command}"
        );
    }

    for command in [
        "git clean --dry-run",
        "git clean -nd",
        "git clean -nfeignored",
        "git -C nested clean --dry-run",
        "git rm --dry-run tracked.txt",
        "git rm -n -- tracked.txt",
        "git rm --help",
        "git clean --help",
        "git clean -h",
        "git clean -hf",
        "git clean -fh",
        "git rm -h tracked.txt",
        "git rm -hf tracked.txt",
        "git reset -h --hard",
        "git reset -hq --hard",
        "git -C clean status",
        "git --unknown clean -fd",
        "rtk rm -rf generated",
    ] {
        assert_eq!(command_risk_note(command), None, "{command}");
    }
    assert_eq!(
        destructive_effect("git rm -- -n"),
        Some(DestructiveEffect::RemoveFiles)
    );
    assert_eq!(
        destructive_effect("git clean -f -- -n"),
        Some(DestructiveEffect::RemoveFiles)
    );
    for command in [
        "git clean -f -e --dry-run",
        "git clean -f --exclude --dry-run",
        "git clean -f -e-n",
        "git clean -fe-n",
        "git clean -f --exclude=--dry-run",
        "git clean -f -ehelp",
        "git clean -f -fehelp",
    ] {
        assert_eq!(
            destructive_effect(command),
            Some(DestructiveEffect::RemoveFiles),
            "{command}"
        );
    }
    assert_eq!(
        destructive_effect("git reset --hard HEAD~1"),
        Some(DestructiveEffect::DiscardVersionControlState)
    );
}

#[test]
fn command_destructive_effect_leaves_unsupported_and_targetless_removal_unresolved() {
    for command in [
        "rm",
        "rm -f",
        "rm --help",
        "rm --version",
        "rm # no target",
        "rm -- # no target",
        "rmdir --verbose",
        "unlink --help",
        "shred -n 3",
        "git rm # no target",
        "git rm -- # no target",
        "git reset # --hard",
        "rm -f; printf ok",
        "git rm --dry-run; printf ok",
        "rm -f < input.txt",
        "rm victim > output.txt",
        "printf ok # harmless; rm victim",
        "cat <<EOF\nrm victim\nEOF",
    ] {
        assert_eq!(command_risk_note(command), None, "{command:?}");
    }

    for command in [
        "rm -- -n",
        "git clean -f # --dry-run",
        "printf ok # ignored; rm first\nrm second",
        "printf foo\\ #bar; rm victim",
        "printf foo\\;#bar; rm victim",
        "printf foo\\\n#bar; rm victim",
    ] {
        assert_eq!(
            destructive_effect(command),
            Some(DestructiveEffect::RemoveFiles),
            "{command:?}"
        );
    }
    assert_eq!(destructive_effect("printf \\\n# comment; rm ignored"), None);
    assert_eq!(
        destructive_effect("git reset --hard; printf ok"),
        Some(DestructiveEffect::DiscardVersionControlState)
    );
    assert_eq!(
        destructive_effect("rm victim; printf ok"),
        Some(DestructiveEffect::RemoveFiles)
    );
}

#[test]
fn command_risk_note_detects_privilege_wrapped_removal() {
    for command in ["sudo rm -rf /tmp", "doas rm -rf /tmp"] {
        assert_eq!(
            command_risk_note(command),
            Some("note: command may remove files forcefully"),
            "{command}"
        );
    }
}

#[test]
fn command_risk_note_detects_privilege_wrapped_git_reset() {
    assert_eq!(
        command_risk_note("sudo git reset --hard"),
        Some("note: command may discard version-control state")
    );
}

#[test]
fn command_risk_note_detects_quoted_su_command_payload() {
    assert_eq!(
        command_risk_note("su -c 'rm -rf /tmp'"),
        Some("note: command may remove files forcefully")
    );
    assert_eq!(
        command_risk_note("su -c \"git reset --hard\""),
        Some("note: command may discard version-control state")
    );
    assert_eq!(
        command_risk_note("su root -c 'rm -rf /tmp'"),
        Some("note: command may remove files forcefully")
    );
}

#[test]
fn command_risk_note_fails_closed_for_malformed_su_command_payload() {
    assert_eq!(command_risk_note("su -c 'rm -rf /tmp"), None);
    assert_eq!(command_risk_note("su -c \"git reset --hard"), None);
}

#[test]
fn command_risk_note_detects_process_control_wrapped_removal() {
    assert!(command_risk_note("nice rm -rf /tmp").is_some());
}

#[test]
fn command_risk_note_detects_timeout_wrapped_git_reset() {
    assert!(command_risk_note("timeout 5 git reset --hard").is_some());
}

#[test]
fn command_risk_note_detects_safe_env_wrapped_git_reset() {
    assert!(command_risk_note("env NODE_ENV=prod git reset --hard").is_some());
}

#[test]
fn command_risk_note_sees_removal_after_unknown_env_prefix() {
    assert_eq!(
        command_risk_note("MY_SECRET=x rm -rf /tmp"),
        Some("note: command may remove files forcefully")
    );
}

#[test]
fn command_risk_note_ignores_quoted_command_text() {
    assert_eq!(command_risk_note("echo 'rm -rf /tmp'"), None);
}

#[test]
fn command_risk_note_ignores_command_text_in_argument_string() {
    assert_eq!(command_risk_note("git commit -m \"rm -rf /tmp\""), None);
}

#[test]
fn command_risk_note_detects_command_after_sequence_boundary() {
    assert!(command_risk_note("echo ok; rm -f scratch").is_some());
}

#[test]
fn multibyte_text_is_scanned_byte_for_byte() {
    assert_eq!(
        command_risk_note("rm café.txt"),
        Some("note: command may remove files forcefully")
    );
    assert_eq!(
        command_risk_note("échec; rm -- é"),
        Some("note: command may remove files forcefully")
    );
    assert_eq!(command_risk_note("su -c 'é'"), None);
    assert_eq!(
        command_safer_alternative("cat naïve.txt"),
        Some("safer: use read_file for file inspection")
    );
    assert_eq!(command_safer_alternative("çat file"), None);
}
