use crate::footer::input_presentation::ComposerView;
use crate::row_text::Row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveParts<'a> {
    pub(crate) provisional: &'a [Row],
    pub(crate) tail_gap: bool,
    pub(crate) activity: Vec<Row>,
    pub(crate) banner: Vec<Row>,
    pub(crate) composer: &'a ComposerView,
    pub(crate) menu: Vec<Row>,
    pub(crate) hint: Option<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveLayout {
    pub(crate) rows: Vec<Row>,
    pub(crate) cursor: Option<(usize, usize)>,
    pub(crate) footer_row: usize,
    pub(crate) composer_start: usize,
}

pub(crate) fn solve(parts: LiveParts<'_>, max_rows: usize) -> LiveLayout {
    let mut footer = parts.banner;
    if !footer.is_empty() {
        footer.push(Row::new());
    }
    let banner_rows = footer.len();
    let composer_rows = parts.composer.rows.len();
    footer.extend(parts.composer.rows.iter().cloned());
    if parts.menu.is_empty() {
        footer.push(Row::new());
    } else {
        footer.extend(parts.menu);
    }
    footer.extend(parts.hint);
    let visible = &parts.provisional[parts.provisional.len().saturating_sub(max_rows)..];
    let mut body = visible.to_vec();
    let mut leading_gaps = visible.len();
    if parts.activity.is_empty() {
        if parts.tail_gap {
            body.push(Row::new());
            leading_gaps += 1;
        }
    } else {
        let body_room = max_rows.saturating_sub(footer.len());
        if body_room >= parts.activity.len() + 2 {
            body.push(Row::new());
            body.extend(parts.activity);
            body.push(Row::new());
            leading_gaps += 1;
        } else {
            body.extend(parts.activity.into_iter().take(body_room));
        }
    }
    let mut rows = body;
    let footer_start = rows.len();
    rows.extend(footer);
    let overflow = rows.len().saturating_sub(max_rows);
    let dropped = overflow.min(footer_start);
    rows.drain(..dropped);
    let footer_start = footer_start - dropped;
    let composer_start = footer_start + banner_rows;
    let cursor = parts.composer.cursor.map(|(row, col)| {
        (
            composer_start + row.min(composer_rows.saturating_sub(1)),
            col,
        )
    });
    LiveLayout {
        rows,
        cursor,
        footer_row: leading_gaps.saturating_sub(dropped),
        composer_start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> ComposerView {
        ComposerView {
            rows: vec![Row::plain("┃ ")],
            cursor: Some((0, 2)),
            review: None,
        }
    }

    fn parts<'a>(
        composer: &'a ComposerView,
        tail_gap: bool,
        activity: &[&str],
        banner: &[&str],
    ) -> LiveParts<'a> {
        LiveParts {
            provisional: &[],
            tail_gap,
            activity: activity.iter().map(|text| Row::plain(text)).collect(),
            banner: banner.iter().map(|text| Row::plain(text)).collect(),
            composer,
            menu: Vec::new(),
            hint: Some(Row::plain("auto · m")),
        }
    }

    fn texts(layout: &LiveLayout) -> Vec<String> {
        layout.rows.iter().map(Row::text).collect()
    }

    #[test]
    fn idle_frames_place_the_composer_above_a_blank_row_and_the_status() {
        let layout = solve(parts(&prompt(), false, &[], &[]), 30);
        assert_eq!(texts(&layout), ["┃ ", "", "auto · m"]);
        assert_eq!(layout.cursor, Some((0, 2)));
        assert_eq!(layout.footer_row, 0);
        let layout = solve(parts(&prompt(), true, &[], &[]), 30);
        assert_eq!(texts(&layout), ["", "┃ ", "", "auto · m"]);
        assert_eq!(layout.cursor, Some((1, 2)));
        assert_eq!(layout.footer_row, 1);
    }

    #[test]
    fn activity_rows_sit_between_blank_gaps_above_the_footer() {
        let layout = solve(parts(&prompt(), true, &["• Thinking (0s)"], &[]), 30);
        assert_eq!(
            texts(&layout),
            ["", "• Thinking (0s)", "", "┃ ", "", "auto · m"]
        );
        assert_eq!(layout.cursor, Some((3, 2)));
        assert_eq!(layout.footer_row, 1);
    }

    #[test]
    fn held_prompts_show_above_the_composer() {
        let layout = solve(parts(&prompt(), false, &["• Generating"], &["┋ next"]), 30);
        assert_eq!(
            texts(&layout),
            ["", "• Generating", "", "┋ next", "", "┃ ", "", "auto · m"]
        );
        assert_eq!(layout.cursor, Some((5, 2)));
        assert_eq!(layout.composer_start, 5);
    }

    #[test]
    fn a_footer_menu_takes_the_place_of_the_gap_above_the_status_line() {
        let composer = prompt();
        let mut parts = parts(&composer, false, &[], &[]);
        parts.menu = vec![
            Row::plain("──"),
            Row::plain("  src/main.rs"),
            Row::plain("──"),
        ];
        let layout = solve(parts, 30);
        assert_eq!(
            texts(&layout),
            ["┃ ", "──", "  src/main.rs", "──", "auto · m"]
        );
        assert_eq!(layout.cursor, Some((0, 2)));
    }

    #[test]
    fn provisional_rows_taller_than_the_screen_keep_their_last_rows() {
        let composer = prompt();
        let provisional: Vec<Row> = (0..50)
            .map(|index| Row::plain(&format!("p{index}")))
            .collect();
        for (activity, expected, footer_row) in [
            (
                &[][..],
                &["p47", "p48", "p49", "", "┃ ", "", "auto · m"][..],
                4,
            ),
            (
                &["• Running"][..],
                &["p48", "p49", "", "• Running", "", "┃ ", "", "auto · m"][..],
                3,
            ),
        ] {
            let layout = solve(
                LiveParts {
                    provisional: &provisional,
                    ..parts(&composer, true, activity, &[])
                },
                expected.len(),
            );
            assert_eq!(texts(&layout), expected);
            assert_eq!(layout.footer_row, footer_row);
            assert_eq!(layout.composer_start, expected.len() - 3);
        }
    }

    #[test]
    fn a_menu_without_a_hint_takes_the_place_of_the_status_line() {
        let composer = prompt();
        let mut parts = parts(&composer, false, &[], &[]);
        parts.menu = vec![
            Row::new(),
            Row::plain("Sessions 1"),
            Row::new(),
            Row::plain("  one"),
        ];
        parts.hint = None;
        let layout = solve(parts, 30);
        assert_eq!(texts(&layout), ["┃ ", "", "Sessions 1", "", "  one"]);
        assert_eq!(layout.cursor, Some((0, 2)));
    }

    #[test]
    fn short_terminals_keep_the_footer_and_drop_activity_gaps() {
        let layout = solve(parts(&prompt(), true, &["• Thinking"], &[]), 4);
        assert_eq!(texts(&layout), ["• Thinking", "┃ ", "", "auto · m"]);
        assert_eq!(layout.cursor, Some((1, 2)));
    }
}
