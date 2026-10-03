use super::*;

fn grid(cols: u16, rows: u16) -> Grid {
    Grid::new(cols, rows).unwrap()
}

fn fed(cols: u16, rows: u16, bytes: &[u8]) -> Grid {
    let mut grid = grid(cols, rows);
    grid.feed(bytes).unwrap();
    grid
}

fn row(grid: &Grid, row: u16) -> String {
    let mut text = Vec::new();
    grid.row_text(row, &mut text);
    String::from_utf8(text)
        .unwrap()
        .trim_end_matches(' ')
        .to_owned()
}

fn cell(grid: &Grid, row: u16, col: u16) -> Cell {
    grid.cells[grid.cell_index(row, col)]
}

fn suffix_of(grid: &Grid, row: u16, col: u16) -> &[u8] {
    grid.combining_suffix(cell(grid, row, col).suffix)
}

fn assert_wide_cell_invariant(grid: &Grid) {
    for row in 1..=grid.rows {
        for col in 1..=grid.cols {
            let current = cell(grid, row, col);
            match current.width {
                0 => {
                    assert!(col > 1, "row {row} col {col}");
                    assert_eq!(cell(grid, row, col - 1).width, 2, "row {row} col {col}");
                    assert_eq!(current.codepoint, 0, "row {row} col {col}");
                }
                1 => {}
                2 => {
                    assert!(col < grid.cols, "row {row} col {col}");
                    let continuation = cell(grid, row, col + 1);
                    assert_eq!(continuation.width, 0, "row {row} col {col}");
                    assert_eq!(continuation.codepoint, 0, "row {row} col {col}");
                }
                width => panic!("width {width} at row {row} col {col}"),
            }
        }
    }
}

#[test]
fn a_grid_needs_a_size_within_the_cell_limit() {
    assert_eq!(Grid::new(0, 1).err(), Some(GridError::InvalidGridSize));
    assert_eq!(Grid::new(1, 0).err(), Some(GridError::InvalidGridSize));
    assert_eq!(Grid::new(1024, 257).err(), Some(GridError::InvalidGridSize));
    assert!(Grid::new(1024, 256).is_ok());
    let mut grid = grid(4, 2);
    assert_eq!(grid.resize(0, 2), Err(GridError::InvalidGridSize));
    assert_eq!(grid.resize(4096, 65), Err(GridError::InvalidGridSize));
    assert_eq!((grid.cols(), grid.rows()), (4, 2));
}

#[test]
fn plain_writes_land_on_the_grid() {
    let grid = fed(10, 3, b"hello");
    assert_eq!(row(&grid, 1), "hello");
    assert_eq!((grid.cursor_row(), grid.cursor_col()), (1, 6));
}

#[test]
fn cup_moves_the_cursor() {
    let grid = fed(10, 4, b"\x1b[2;3Hab");
    assert_eq!(row(&grid, 2), "  ab");
    assert_eq!((grid.cursor_row(), grid.cursor_col()), (2, 5));
}

#[test]
fn lf_advances_to_the_next_row_at_column_one() {
    let grid = fed(10, 3, b"a\nb\nc");
    assert_eq!(
        [row(&grid, 1), row(&grid, 2), row(&grid, 3)],
        ["a", "b", "c"]
    );
}

#[test]
fn cr_returns_to_column_one_without_advancing() {
    assert_eq!(row(&fed(10, 2, b"abc\rXY"), 1), "XYc");
}

#[test]
fn el_clears_the_line_or_its_end() {
    let mut whole = fed(10, 2, b"hello world");
    whole.feed(b"\x1b[1;1H\x1b[2K").unwrap();
    assert_eq!(row(&whole, 1), "");
    let mut tail = fed(10, 2, b"hello");
    tail.feed(b"\x1b[1;3H\x1b[K").unwrap();
    assert_eq!(row(&tail, 1), "he");
}

#[test]
fn erasing_from_inside_a_wide_glyph_erases_all_of_it() {
    let mut continuation = fed(4, 1, "ab界".as_bytes());
    continuation.feed(b"\x1b[1;4H\x1b[K").unwrap();
    assert_wide_cell_invariant(&continuation);
    assert_eq!(row(&continuation, 1), "ab");

    let mut lead = fed(4, 1, "界xy".as_bytes());
    lead.feed(b"\x1b[1;1H\x1b[1K").unwrap();
    assert_wide_cell_invariant(&lead);
    assert_eq!(row(&lead, 1), "  xy");

    let mut display = fed(4, 2, "ab界".as_bytes());
    display.feed(b"\x1b[1;4H\x1b[J").unwrap();
    assert_wide_cell_invariant(&display);
    assert_eq!([row(&display, 1), row(&display, 2)], ["ab", ""]);

    let mut above = fed(4, 2, "\x1b[2;1H界xy".as_bytes());
    above.feed(b"\x1b[2;1H\x1b[1J").unwrap();
    assert_wide_cell_invariant(&above);
    assert_eq!([row(&above, 1), row(&above, 2)], ["", "  xy"]);
}

#[test]
fn ed_clears_to_the_end_or_the_whole_display() {
    let mut below = fed(5, 3, b"\x1b[1;1Haaaaa\x1b[2;1Hbbbbb\x1b[3;1Hccccc");
    below.feed(b"\x1b[2;1H\x1b[J").unwrap();
    assert_eq!(
        [row(&below, 1), row(&below, 2), row(&below, 3)],
        ["aaaaa", "", ""]
    );
    let mut whole = fed(4, 2, b"\x1b[1;1Hab\x1b[2;1Hcd");
    whole.feed(b"\x1b[2J").unwrap();
    assert_eq!([row(&whole, 1), row(&whole, 2)], ["", ""]);
    let mut scrollback = fed(4, 2, b"ab");
    scrollback.feed(b"\x1b[3J").unwrap();
    assert_eq!(row(&scrollback, 1), "ab");
}

#[test]
fn autowrap_wraps_past_the_margin_until_decrst_seven() {
    let mut grid = fed(4, 3, b"abcdef");
    assert_eq!([row(&grid, 1), row(&grid, 2)], ["abcd", "ef"]);
    grid.feed(b"\x1b[3;1H\x1b[?7lXXXXXYY").unwrap();
    assert_eq!(row(&grid, 3), "XXXY");
}

#[test]
fn a_tab_clears_pending_wrap_and_uses_the_next_stop() {
    let margin = fed(8, 2, b"12345678\tX");
    assert_eq!([row(&margin, 1), row(&margin, 2)], ["1234567X", ""]);
    assert_eq!((margin.cursor_row(), margin.cursor_col()), (1, 8));
    assert!(margin.cursor.pending_wrap);

    let ordinary = fed(16, 1, b"a\tb");
    assert_eq!(row(&ordinary, 1), "a       b");
    assert_eq!(ordinary.cursor_col(), 10);
}

#[test]
fn combining_marks_keep_the_base_characters_wrap_geometry() {
    let base = fed(4, 2, b"eeeee");
    let decomposed = fed(4, 2, "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}".as_bytes());
    assert_eq!(
        (
            base.cursor_row(),
            base.cursor_col(),
            base.cursor.pending_wrap
        ),
        (
            decomposed.cursor_row(),
            decomposed.cursor_col(),
            decomposed.cursor.pending_wrap
        )
    );
    assert_eq!(row(&decomposed, 1), "e\u{301}e\u{301}e\u{301}e\u{301}");
}

#[test]
fn combining_marks_stay_with_the_base_cell() {
    let grid = fed(8, 1, "e\u{301}\u{327}x".as_bytes());
    assert_eq!(row(&grid, 1), "e\u{301}\u{327}x");
    assert_eq!(grid.cursor_col(), 3);
}

#[test]
fn repeated_combining_suffixes_share_storage() {
    let grid = fed(8, 1, "e\u{301}e\u{301}e\u{301}".as_bytes());
    assert_eq!(grid.combining_pool.len(), 1);
}

#[test]
fn a_combining_suffix_clears_with_its_cell() {
    let mut grid = fed(8, 1, "e\u{301}".as_bytes());
    grid.feed(b"\x1b[1;1Hx").unwrap();
    assert_eq!(row(&grid, 1), "x");
    assert_eq!(cell(&grid, 1, 1).suffix, 0);
    grid.feed("\x1b[1;1He\u{301}\x1b[2K".as_bytes()).unwrap();
    assert_eq!(row(&grid, 1), "");
    assert_eq!(cell(&grid, 1, 1).suffix, 0);
}

#[test]
fn combining_marks_attach_to_a_wide_lead_at_pending_wrap() {
    let grid = fed(3, 1, "a界\u{301}".as_bytes());
    assert_eq!(row(&grid, 1), "a界\u{301}");
    assert!(grid.cursor.pending_wrap);
    assert_eq!(grid.cursor_col(), 3);
    assert_ne!(cell(&grid, 1, 2).suffix, 0);
    assert_eq!(cell(&grid, 1, 3).suffix, 0);
}

#[test]
fn display_units_keep_their_bytes_and_geometry() {
    let cases: [(&str, u8); 8] = [
        ("\u{2600}\u{FE0E}", 1),
        ("\u{231A}\u{FE0E}", 2),
        ("\u{2600}\u{FE0F}", 2),
        ("\u{1F44D}\u{1F3FD}", 2),
        ("\u{1F1FA}\u{1F1F8}", 2),
        ("#\u{FE0F}\u{20E3}", 2),
        (
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
            2,
        ),
        ("\u{1F469}\u{200D}\u{1F4BB}", 2),
    ];
    for (text, width) in cases {
        let mut grid = grid(16, 1);
        grid.feed(b"A").unwrap();
        grid.feed(text.as_bytes()).unwrap();
        grid.feed(b"B").unwrap();
        assert_eq!(row(&grid, 1), format!("A{text}B"), "{text:?}");
        assert_eq!(grid.cursor_col(), u16::from(width) + 3, "{text:?}");
        let lead = cell(&grid, 1, 2);
        let first = text.chars().next().unwrap();
        assert_eq!(lead.width, width, "{text:?}");
        assert_eq!(lead.codepoint, u32::from(first), "{text:?}");
        assert_eq!(
            suffix_of(&grid, 1, 2),
            &text.as_bytes()[first.len_utf8()..],
            "{text:?}"
        );
    }
}

#[test]
fn display_unit_suffixes_clear_on_overwrite_and_erase() {
    for text in [
        "\u{2600}\u{FE0E}",
        "\u{1F44D}\u{1F3FD}",
        "\u{1F469}\u{200D}\u{1F4BB}",
    ] {
        let mut grid = fed(8, 1, text.as_bytes());
        grid.feed(b"\x1b[1;1HX").unwrap();
        assert_eq!(row(&grid, 1), "X");
        assert_eq!(cell(&grid, 1, 1).suffix, 0);
        grid.feed(b"\x1b[1;1H").unwrap();
        grid.feed(text.as_bytes()).unwrap();
        grid.feed(b"\x1b[1;1H\x1b[2K").unwrap();
        assert_eq!(row(&grid, 1), "");
        assert_eq!(cell(&grid, 1, 1).suffix, 0);
    }
}

#[test]
fn display_units_survive_resize_while_intact_and_clear_when_clipped() {
    let flag = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
    let mut grid = fed(3, 1, format!("{flag}x").as_bytes());
    grid.resize(6, 2).unwrap();
    assert_eq!(row(&grid, 1), format!("{flag}x"));
    grid.resize(3, 1).unwrap();
    assert_eq!(row(&grid, 1), format!("{flag}x"));
    grid.resize(1, 1).unwrap();
    assert_eq!(row(&grid, 1), "");
    assert_eq!(cell(&grid, 1, 1).suffix, 0);
}

#[test]
fn sgr_osc_and_queries_leave_the_text_alone() {
    assert_eq!(
        row(&fed(8, 1, b"\x1b[38;5;240m[sys]\x1b[0m x"), 1),
        "[sys] x"
    );
    assert_eq!(row(&fed(6, 1, b"\x1b]2;my title\x07text"), 1), "text");
    assert_eq!(
        row(&fed(6, 1, b"\x1b]11;rgb:0000/0000/0000\x1b\\hello"), 1),
        "hello"
    );
    let mut query = fed(6, 1, b"\x1b[6n");
    query.feed(b"ok").unwrap();
    assert_eq!(row(&query, 1), "ok");
    let replies = fed(
        12,
        1,
        b"\x1b[5n\x1b[c\x1b[>c\x1b[18t\x1bP$qm\x1b\\\x1b[2 q\x1b[>4;2m\x1b[>1uok",
    );
    assert_eq!(row(&replies, 1), "ok");
}

#[test]
fn resize_keeps_the_top_left_and_clips_the_rest() {
    let mut grown = fed(4, 2, b"abcd\nef");
    grown.resize(6, 3).unwrap();
    assert_eq!(
        [row(&grown, 1), row(&grown, 2), row(&grown, 3)],
        ["abcd", "ef", ""]
    );
    assert_eq!((grown.cols(), grown.rows()), (6, 3));

    let mut shrunk = fed(6, 3, b"\x1b[1;1Haaabbb\x1b[2;1Hxxxxxx\x1b[3;1Hyyyyyy");
    shrunk.resize(4, 2).unwrap();
    assert_eq!([row(&shrunk, 1), row(&shrunk, 2)], ["aaab", "xxxx"]);
    assert_eq!((shrunk.cols(), shrunk.rows()), (4, 2));

    let mut clipped = fed(4, 1, "ab界".as_bytes());
    clipped.resize(3, 1).unwrap();
    assert_wide_cell_invariant(&clipped);
    assert_eq!(row(&clipped, 1), "ab");

    let mut cursor = fed(8, 4, b"\x1b[4;8H");
    cursor.resize(4, 2).unwrap();
    assert_eq!((cursor.cursor_row(), cursor.cursor_col()), (2, 4));
}

#[test]
fn writes_clear_any_wide_glyph_they_overlap() {
    for (initial, col, replacement) in [("界xy", 1, "a"), ("界xy", 2, "a"), ("界zz", 2, "界")] {
        let mut grid = fed(4, 1, initial.as_bytes());
        grid.feed(format!("\x1b[1;{col}H{replacement}").as_bytes())
            .unwrap();
        assert_wide_cell_invariant(&grid);
    }
}

#[test]
fn a_line_feed_on_the_last_row_scrolls() {
    let mut grid = fed(4, 2, b"AAAA\nBBBB");
    grid.feed(b"\nCCCC").unwrap();
    assert_eq!([row(&grid, 1), row(&grid, 2)], ["BBBB", "CCCC"]);
}

#[test]
fn repeated_scroll_rotations_keep_logical_rows_through_resize_and_erase() {
    let mut grid = fed(5, 3, b"one\ntwo\nthree\nfour\nfive");
    assert_eq!(
        [row(&grid, 1), row(&grid, 2), row(&grid, 3)],
        ["three", "four", "five"]
    );
    let mut erased = fed(5, 3, b"one\ntwo\nthree\nfour\nfive");
    erased.feed(b"\x1b[2;2H\x1b[K").unwrap();
    assert_eq!(
        [row(&erased, 1), row(&erased, 2), row(&erased, 3)],
        ["three", "f", "five"]
    );
    grid.resize(7, 4).unwrap();
    assert_eq!(
        [row(&grid, 1), row(&grid, 2), row(&grid, 3), row(&grid, 4)],
        ["three", "four", "five", ""]
    );
    assert_eq!(grid.row_origin, 0);
}

#[test]
fn the_alternate_screen_restores_the_normal_screen() {
    let mut rotated = fed(5, 3, b"one\ntwo\nthree\nfour");
    rotated.feed(b"\x1b[?1049halternate\x1b[?1049l").unwrap();
    assert_eq!(
        [row(&rotated, 1), row(&rotated, 2), row(&rotated, 3)],
        ["two", "three", "four"]
    );

    let mut grid = fed(8, 2, b"normal");
    grid.feed(b"\x1b[?25l\x1b[?1049h").unwrap();
    assert!(!grid.cursor_visible());
    grid.feed(b"approval").unwrap();
    assert_eq!(row(&grid, 1), "approval");
    grid.feed(b"\x1b[?25h\x1b[?1049l").unwrap();
    assert!(grid.cursor_visible());
    assert_eq!(row(&grid, 1), "normal");

    let mut resized = fed(8, 2, b"normal");
    resized.feed(b"\x1b[?1049happroval").unwrap();
    resized.resize(12, 3).unwrap();
    resized.feed(b"\x1b[2;1Hresized approval").unwrap();
    resized.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(row(&resized, 1), "normal");
    assert_eq!((resized.cols(), resized.rows()), (12, 3));

    for mode in ["47", "1047"] {
        let mut legacy = fed(8, 1, b"normal");
        legacy
            .feed(format!("\x1b[?{mode}halt\x1b[?{mode}l").as_bytes())
            .unwrap();
        assert_eq!(row(&legacy, 1), "normal", "{mode}");
    }
}

#[test]
fn wide_and_combining_cells_survive_row_rotations() {
    let grid = fed(6, 2, "界e\u{301}\nplain\n界e\u{301}".as_bytes());
    assert_wide_cell_invariant(&grid);
    assert_eq!([row(&grid, 1), row(&grid, 2)], ["plain", "界e\u{301}"]);
}

#[test]
fn the_snapshot_frames_every_row() {
    let grid = fed(3, 2, b"\x1b[1;1Hab\x1b[2;1Hcd");
    assert_eq!(grid.snapshot(), b"|ab |\n|cd |\n");
    assert_eq!(fed(3, 1, "界".as_bytes()).snapshot(), "|界 |\n".as_bytes());
}

#[test]
fn synchronized_updates_wait_for_decrst() {
    let mut grid = fed(4, 2, b"\x1b[?2026h");
    grid.feed(b"ab").unwrap();
    assert_eq!(row(&grid, 1), "");
    grid.feed(b"\x1b[?2026l").unwrap();
    assert_eq!(row(&grid, 1), "ab");

    let mut split = fed(4, 2, b"x\x1b[?2026hab\x1b[?20");
    assert_eq!(row(&split, 1), "x");
    split.feed(b"26lcd").unwrap();
    assert_eq!(row(&split, 1), "xabc");
    assert_eq!(row(&split, 2), "d");
}

#[test]
fn a_synchronized_update_is_bounded() {
    let mut grid = fed(4, 2, b"\x1b[?2026h");
    let large = vec![b'x'; 1024 * 1024];
    grid.feed(&large).unwrap();
    assert_eq!(grid.feed(b"y"), Err(GridError::SynchronizedUpdateTooLarge));
}

#[test]
fn a_partial_csi_continues_in_the_next_feed() {
    let mut grid = fed(6, 2, b"\x1b[2;");
    grid.feed(b"3Hx").unwrap();
    assert_eq!(row(&grid, 2), "  x");
}

#[test]
fn can_and_sub_cancel_a_partial_csi_or_osc() {
    let mut grid = fed(12, 2, b"\x1b[2;");
    grid.feed(b"\x18\x1b[1;1Hcsi").unwrap();
    assert_eq!(row(&grid, 1), "csi");
    grid.feed(b"\x1b]8;;https://bad.example").unwrap();
    grid.feed(b"\x1aplain").unwrap();
    assert_eq!(row(&grid, 1), "csiplain");
}

#[test]
fn editing_scroll_regions_origin_mode_and_saved_cursors_follow_upstream() {
    let mut grid = fed(8, 4, b"abcdefgh\x1b[1;3H\x1b[2@XY\x1b[P\x1b[2X");
    assert_eq!(row(&grid, 1), "abXY  f");

    grid.feed(b"\x1b[2;4r\x1b[?6h\x1b[1;1HA\nB\nC\nD").unwrap();
    assert_eq!((grid.scroll_top, grid.scroll_bottom), (2, 4));
    assert!(grid.modes.origin);
    assert_eq!(
        [row(&grid, 2), row(&grid, 3), row(&grid, 4)],
        ["B", "C", "D"]
    );

    grid.feed(b"\x1b7\x1b[4;5HZ\x1b8Q").unwrap();
    assert_eq!(cell(&grid, 4, 2).codepoint, u32::from('Q'));
    grid.feed(b"\x1b[?7l\x1b[4h").unwrap();
    assert!(!grid.modes.autowrap);
    assert!(grid.modes.insert);
}

#[test]
fn line_and_scroll_commands_stay_inside_the_scroll_region() {
    let mut grid = fed(4, 4, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    grid.feed(b"\x1b[2;3r\x1b[2;1H\x1b[L").unwrap();
    assert_eq!(
        [row(&grid, 1), row(&grid, 2), row(&grid, 3), row(&grid, 4)],
        ["aaaa", "", "bbbb", "dddd"]
    );
    grid.feed(b"\x1b[M").unwrap();
    assert_eq!(
        [row(&grid, 1), row(&grid, 2), row(&grid, 3), row(&grid, 4)],
        ["aaaa", "bbbb", "", "dddd"]
    );
    grid.feed(b"\x1b[S").unwrap();
    assert_eq!([row(&grid, 2), row(&grid, 3)], ["", ""]);
    grid.feed(b"\x1b[3;1Hx\x1b[T").unwrap();
    assert_eq!([row(&grid, 2), row(&grid, 3)], ["", ""]);
    grid.feed(b"\x1b[2;1HM\x1bMy").unwrap();
    assert_eq!(
        [row(&grid, 1), row(&grid, 2), row(&grid, 3), row(&grid, 4)],
        ["aaaa", " y", "M", "dddd"]
    );
}

#[test]
fn cursor_movement_commands_clamp_to_the_screen() {
    let mut grid = grid(10, 5);
    for (sequence, expected) in [
        ("\x1b[3;4H", (3, 4)),
        ("\x1b[9A", (1, 4)),
        ("\x1b[2B", (3, 4)),
        ("\x1b[20C", (3, 10)),
        ("\x1b[3D", (3, 7)),
        ("\x1b[E", (4, 1)),
        ("\x1b[2F", (2, 1)),
        ("\x1b[6G", (2, 6)),
        ("\x1b[5`", (2, 5)),
        ("\x1b[9d", (5, 5)),
        ("\x1b[2a", (5, 7)),
        ("\x1b[e", (5, 7)),
        ("\x1b[0;0f", (1, 1)),
        ("\x1bE", (2, 1)),
        ("\x1bD", (3, 1)),
        ("\x1b[I", (3, 9)),
        ("\x1b[Z", (3, 1)),
        ("ab\x08", (3, 2)),
        ("\x1b[s\x1b[5;5H\x1b[u", (3, 2)),
    ] {
        grid.feed(sequence.as_bytes()).unwrap();
        assert_eq!(
            (grid.cursor_row(), grid.cursor_col()),
            expected,
            "{sequence:?}"
        );
    }
}

#[test]
fn tab_stops_can_be_set_and_cleared() {
    let mut grid = fed(12, 3, b"A\tB\x1bH\r\tC");
    assert_eq!(cell(&grid, 1, 9).codepoint, u32::from('C'));
    grid.feed(b"\r\n\x1b[3g\tD").unwrap();
    assert_eq!(cell(&grid, 2, 12).codepoint, u32::from('D'));
    grid.feed(b"\r\n\x1b[5G\x1bH\x1b[1G\x1b[0g\tE").unwrap();
    assert_eq!(cell(&grid, 3, 5).codepoint, u32::from('E'));
}

#[test]
fn the_alternate_screen_and_fragmented_utf8_keep_the_invariants() {
    let mut grid = fed(12, 3, b"A\tB");
    grid.feed(b"\x1b[?1049hALT\x1b[?1049l").unwrap();
    assert_eq!(cell(&grid, 1, 1).codepoint, u32::from('A'));
    grid.feed(b"\xe7\x95").unwrap();
    grid.feed(b"\x8ce").unwrap();
    grid.feed(b"\xcc\x81").unwrap();
    assert_wide_cell_invariant(&grid);
    assert_eq!(row(&grid, 1), "A       B界e\u{301}");
}

#[test]
fn invalid_utf8_becomes_replacement_characters() {
    assert_eq!(row(&fed(8, 1, b"a\xffb\x80c"), 1), "a\u{fffd}b\u{fffd}c");
    let mut split = fed(8, 1, b"x\xe7");
    split.feed(b"y").unwrap();
    assert_eq!(row(&split, 1), "x\u{fffd}y");
    let mut surrogate = fed(8, 1, b"q\xed\xa0");
    surrogate.feed(b"\x80").unwrap();
    assert_eq!(row(&surrogate, 1), "q\u{fffd}");
}

#[test]
fn reset_restores_the_initial_state() {
    let mut grid = fed(6, 2, b"\x1b[?1049h\x1b[?7l\x1b[2;2r\x1b[4hab\x1b7");
    grid.feed(b"\x1bc").unwrap();
    assert_eq!((grid.cursor_row(), grid.cursor_col()), (1, 1));
    assert!(grid.modes.autowrap && !grid.modes.insert && !grid.modes.origin);
    assert_eq!((grid.scroll_top, grid.scroll_bottom), (1, 2));
    assert!(grid.saved_normal_screen.is_none() && grid.saved_cursor.is_none());
    grid.feed(b"xyz").unwrap();
    assert_eq!(row(&grid, 1), "xyz");
}

#[test]
fn parser_collections_enforce_their_bounds() {
    let mut params = grid(10, 2);
    assert_eq!(
        params.feed(b"\x1b[1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1H"),
        Err(GridError::TooManyCsiParameters)
    );
    let mut intermediates = grid(10, 2);
    assert_eq!(
        intermediates.feed(b"\x1b[!!!p"),
        Err(GridError::TooManyCsiIntermediates)
    );
    let mut osc = grid(10, 2);
    let mut oversized = b"\x1b]".to_vec();
    oversized.extend(std::iter::repeat_n(b'x', 4096 + 1));
    assert_eq!(osc.feed(&oversized), Err(GridError::ControlStringTooLarge));
    let mut dcs = grid(10, 2);
    let mut oversized = b"\x1bP".to_vec();
    oversized.extend(std::iter::repeat_n(b'x', 4096 + 1));
    assert_eq!(dcs.feed(&oversized), Err(GridError::ControlStringTooLarge));
    let mut combining = grid(10, 2);
    let marks = "\u{301}".repeat(33);
    assert_eq!(
        combining.feed(format!("e{marks}").as_bytes()),
        Err(GridError::CombiningPoolCapacityExceeded)
    );
}

#[test]
fn zero_width_bytes_join_the_previous_cell() {
    assert_eq!(row(&fed(6, 1, b"a\x7fb"), 1), "a\x7fb");
    assert_eq!(row(&fed(6, 1, "\u{301}x".as_bytes()), 1), "x");
}

#[test]
fn fragmented_feeds_match_a_single_feed() {
    let script = "plain \x1b[1;31mred\x1b[0m\r\n界e\u{301}\x1b[2;3H\x1b[K\x1b]0;t\x07\x1b[?2026hsync\x1b[?2026l\x1b[3;1Hz".as_bytes();
    let whole = fed(10, 4, script);
    for chunk in 1..=7 {
        let mut pieces = grid(10, 4);
        for piece in script.chunks(chunk) {
            pieces.feed(piece).unwrap();
        }
        assert_eq!(pieces.snapshot(), whole.snapshot(), "chunk {chunk}");
        assert_eq!(
            (pieces.cursor_row(), pieces.cursor_col()),
            (whole.cursor_row(), whole.cursor_col()),
            "chunk {chunk}"
        );
    }
}
