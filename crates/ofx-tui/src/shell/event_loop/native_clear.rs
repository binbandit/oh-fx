use super::super::test_shell::TestShell;
use crate::render_engine::frame_sink::FrameSink;
use crate::terminal::TAGGED_CURSOR_QUERY;

const CLEAR_IN_FRAME: &str = "\x1b[?2026h\x1b[?25l\x1b[0m\x1b[2J\x1b[3J\x1b[H";

fn probing() -> TestShell {
    let mut test = TestShell::start();
    test.shell.input.start_native_clear_probe();
    test.shell.input_notice("earlier output");
    test.screen();
    test
}

fn cursor_row(test: &TestShell) -> u16 {
    test.shell.renderer.cursor_row().unwrap()
}

fn reply(row: u16) -> Vec<u8> {
    format!("\x1b[{row};1R\x1b[{row};2R").into_bytes()
}

#[test]
fn typed_keys_wait_for_the_reply_pair_and_a_matching_row_keeps_the_screen() {
    let mut test = probing();
    let row = cursor_row(&test);
    test.type_bytes(b"hi");
    test.step();
    assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
    assert!(test.shell.composer.is_empty());
    let now_ms = test.shell.now_ms();
    let deadline_ms = test.shell.next_deadline_ms(now_ms).unwrap();
    assert!(deadline_ms > now_ms && deadline_ms <= now_ms + 100);
    test.type_bytes(&reply(row));
    test.step();
    let written = test.written();
    assert!(!written.contains("\x1b[3J"), "{written:?}");
    let screen = test.screen();
    assert!(screen.contains("earlier output"), "{screen}");
    assert!(screen.contains("┃ hi"), "{screen}");
    test.type_bytes(b"!");
    test.step();
    assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
}

#[test]
fn a_reply_on_another_row_starts_a_fresh_screen_before_the_held_keys() {
    let mut test = probing();
    test.type_bytes(b"x");
    test.step();
    assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
    test.type_bytes(&reply(1));
    test.step();
    let written = test.written();
    assert!(written.starts_with(CLEAR_IN_FRAME), "{written:?}");
    let screen = test.screen();
    assert!(!screen.contains("earlier output"), "{screen}");
    assert!(screen.contains("oh-fx"), "{screen}");
    assert!(screen.contains("┃ x"), "{screen}");
}

#[test]
fn a_terminal_that_never_answers_gets_the_keys_after_the_wait_and_no_more_probes() {
    let mut test = probing();
    test.type_bytes(b"a");
    test.step();
    assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
    test.advance(100);
    test.step();
    assert_eq!(test.shell.composer.text(), "a");
    test.type_bytes(b"b");
    test.step();
    assert!(!test.written().contains(TAGGED_CURSOR_QUERY));
    assert_eq!(test.shell.composer.text(), "ab");
}

#[test]
fn a_pending_resize_or_key_sequence_does_not_start_a_probe() {
    let mut test = probing();
    test.signal_resize(24, 80);
    test.type_bytes(b"a");
    test.step();
    assert!(!test.written().contains(TAGGED_CURSOR_QUERY));
    assert_eq!(test.shell.composer.text(), "a");
    test.advance(100);
    test.step();
    test.screen();
    test.type_bytes(b"\x1b[");
    test.step();
    test.type_bytes(b"D");
    test.step();
    assert!(!test.written().contains(TAGGED_CURSOR_QUERY));
}

#[test]
fn a_resize_during_the_probe_settles_after_the_reply() {
    let mut test = probing();
    let row = cursor_row(&test);
    test.type_bytes(b"a");
    test.step();
    test.signal_resize(24, 80);
    test.advance(150);
    test.settle();
    assert!(test.shell.pending_resize.is_some());
    let now_ms = test.shell.now_ms();
    assert!(test.shell.next_deadline_ms(now_ms).unwrap() > now_ms);
    test.type_bytes(&reply(row));
    test.step();
    test.step();
    assert!(test.shell.pending_resize.is_none());
    assert!(test.screen().contains("┃ a"));
}
