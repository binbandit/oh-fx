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

#[test]
fn a_reply_moved_by_a_resize_during_the_probe_keeps_the_transcript() {
    let mut test = probing();
    for index in 0..30 {
        test.shell.input_notice(&format!("notice {index}"));
    }
    test.shell.mark_dirty();
    test.screen();
    let row = cursor_row(&test);
    assert!(row > 10, "{row}");
    test.type_bytes(b"a");
    test.step();
    assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
    test.signal_resize(10, 80);
    test.type_bytes(&reply(10));
    test.step();
    test.advance(150);
    test.step();
    assert!(test.shell.pending_resize.is_none());
    test.shell.transcript.restart(80);
    let replayed: String = test
        .shell
        .transcript
        .take_new_rows(&test.shell.theme)
        .iter()
        .map(|row| row.text() + "\n")
        .collect();
    assert!(replayed.contains("earlier output"), "{replayed}");
    assert!(replayed.contains("notice 29"), "{replayed}");
    assert_eq!(test.shell.composer.text(), "a");
}

fn pasting_after_a_timed_out_probe() -> TestShell {
    let mut test = probing();
    test.type_bytes(b"a\x1b[200~");
    test.step();
    assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
    test.advance(100);
    test.step();
    test
}

#[test]
fn a_late_reply_right_after_a_paste_ends_leaves_the_paste_whole() {
    let mut test = pasting_after_a_timed_out_probe();
    test.type_bytes(b"text\x1b[201~\x1b[5;1R\x1b[5;2R");
    test.step();
    test.settle();
    assert_eq!(test.shell.composer.text(), "atext");
}

#[test]
fn a_late_reply_inside_a_paste_stays_out_of_the_pasted_text() {
    let mut test = pasting_after_a_timed_out_probe();
    test.type_bytes(b"left\x1b[5;1R\x1b[5;2Rright\x1b[201~");
    test.step();
    test.settle();
    assert_eq!(test.shell.composer.text(), "aleftright");
}

#[test]
fn a_split_suffix_after_the_paste_end_rejects_the_paste_instead_of_submitting_it() {
    let mut test = pasting_after_a_timed_out_probe();
    test.type_bytes(b"safe\x1b[201~\x1b[");
    test.step();
    test.settle();
    test.type_bytes(b"13u");
    test.step();
    test.settle();
    assert!(test.sent().is_empty(), "{:?}", test.sent());
    assert_eq!(test.shell.composer.text(), "a");
}

#[test]
fn a_late_reply_split_across_reads_after_the_paste_end_leaves_the_paste_whole() {
    let mut test = pasting_after_a_timed_out_probe();
    test.type_bytes(b"safe\x1b[201~\x1b[5;");
    test.step();
    test.settle();
    test.type_bytes(b"1R\x1b[5;2R");
    test.step();
    test.settle();
    assert_eq!(test.shell.composer.text(), "asafe");
    assert!(test.sent().is_empty());
}

#[test]
fn a_suffix_that_never_completes_rejects_the_paste_after_the_idle_wait() {
    let mut test = pasting_after_a_timed_out_probe();
    test.type_bytes(b"safe\x1b[201~\x1b[");
    test.step();
    test.settle();
    assert_eq!(test.shell.composer.text(), "a");
    let now_ms = test.shell.now_ms();
    let deadline_ms = test.shell.next_deadline_ms(now_ms).unwrap();
    assert!(deadline_ms <= now_ms + 100, "{deadline_ms} {now_ms}");
    test.advance(100);
    test.step();
    test.settle();
    assert_eq!(test.shell.composer.text(), "a");
    assert!(test.sent().is_empty());
    let screen = test.screen();
    assert!(screen.contains("✗ input:"), "{screen}");
}
