use ofx_contract::{UiCommand, UiEvent};

use crate::shell::test_shell::TestShell;

const ESC: &[u8] = b"\x1b[27u";
const URL: &str = "https://auth.example/oauth/authorize?client_id=x&state=s";

fn press(test: &mut TestShell, keys: &[u8]) {
    test.type_bytes(keys);
    test.step();
}

fn open_sign_in(test: &mut TestShell) {
    test.deliver(UiEvent::SignInStarted {
        url: URL.to_owned(),
    });
}

#[test]
fn the_sign_in_screen_owns_the_footer_and_links_its_authorization() {
    let mut test = TestShell::start();
    open_sign_in(&mut test);
    let written = test.written();
    assert!(
        written.contains(&format!("\x1b]8;id=fx-codex-auth;{URL}\x1b\\")),
        "{written:?}"
    );
    let screen = test.screen();
    let rows: Vec<&str> = screen.lines().collect();
    assert!(
        rows.contains(
            &"Sign in with Codex                                    Waiting for authorization…"
        ),
        "{screen}"
    );
    assert!(rows.contains(&"  Open   Authorize with Codex"), "{screen}");
    assert!(
        rows.contains(&"enter reopens browser · esc cancels"),
        "{screen}"
    );
    assert!(!screen.contains(URL), "{screen}");
    press(&mut test, b"typed");
    assert_eq!(test.shell.composer.text(), "");
    assert!(test.sent().is_empty());
    press(&mut test, b"\r");
    assert_eq!(test.sent(), [UiCommand::ReopenSignIn]);
    press(&mut test, ESC);
    assert_eq!(
        test.sent(),
        [UiCommand::ReopenSignIn, UiCommand::CancelSignIn]
    );
    assert!(!test.screen().contains("Sign in with Codex"));
}

#[test]
fn ctrl_c_and_ctrl_d_cancel_the_sign_in_without_arming_exit() {
    for key in [&b"\x03"[..], b"\x04"] {
        let mut test = TestShell::start();
        open_sign_in(&mut test);
        press(&mut test, key);
        assert_eq!(test.sent(), [UiCommand::CancelSignIn]);
        let screen = test.screen();
        assert!(!screen.contains("Sign in with Codex"), "{screen}");
        assert!(!screen.contains("again to exit"), "{screen}");
        assert!(!test.shell.should_exit);
    }
}

#[test]
fn a_finished_sign_in_closes_its_screen() {
    let mut test = TestShell::start();
    open_sign_in(&mut test);
    test.deliver(UiEvent::SignInEnded);
    assert!(!test.screen().contains("Sign in with Codex"));
    press(&mut test, b"hi");
    assert_eq!(test.shell.composer.text(), "hi");
}
