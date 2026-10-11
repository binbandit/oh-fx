use std::process::{Command, Output, Stdio};

const OH_FX: &str = env!("CARGO_BIN_EXE_oh-fx");

fn hidden_mode(args: &[&str]) -> Output {
    Command::new(OH_FX)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run oh-fx")
}

#[test]
fn the_hidden_terminal_launcher_starts_nothing_without_a_config() {
    let output = hidden_mode(&["--oh-fx-internal-terminal-launcher"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn the_hidden_terminal_control_marker_needs_its_launcher() {
    let nonce = "a".repeat(32);
    let output = hidden_mode(&[
        "--oh-fx-internal-terminal-control",
        "/nonexistent/control.sock",
        &nonce,
        "shell-ready",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
