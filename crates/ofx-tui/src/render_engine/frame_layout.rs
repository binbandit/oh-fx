use crate::footer::input_presentation::ComposerView;
use crate::row_text::Row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveParts<'a> {
    pub(crate) tail_gap: bool,
    pub(crate) activity: Vec<Row>,
    pub(crate) banner: Vec<Row>,
    pub(crate) composer: &'a ComposerView,
    pub(crate) hint: Row,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveLayout {
    pub(crate) rows: Vec<Row>,
    pub(crate) cursor: (usize, usize),
    pub(crate) footer_row: usize,
}

pub(crate) fn solve(parts: LiveParts<'_>, max_rows: usize) -> LiveLayout {
    let mut footer = parts.banner;
    if !footer.is_empty() {
        footer.push(Row::new());
    }
    let banner_rows = footer.len();
    let composer_rows = parts.composer.rows.len();
    footer.extend(parts.composer.rows.iter().cloned());
    footer.push(Row::new());
    footer.push(parts.hint);
    let mut body = Vec::new();
    let mut leading_gaps: usize = 0;
    if parts.activity.is_empty() {
        if parts.tail_gap {
            body.push(Row::new());
            leading_gaps = 1;
        }
    } else {
        let body_room = max_rows.saturating_sub(footer.len());
        if body_room >= parts.activity.len() + 2 {
            body.push(Row::new());
            body.extend(parts.activity);
            body.push(Row::new());
            leading_gaps = 1;
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
    let cursor_row = composer_start + parts.composer.cursor.0.min(composer_rows.saturating_sub(1));
    LiveLayout {
        rows,
        cursor: (cursor_row, parts.composer.cursor.1),
        footer_row: leading_gaps.saturating_sub(dropped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> ComposerView {
        ComposerView {
            rows: vec![Row::plain("┃ ")],
            cursor: (0, 2),
        }
    }

    fn parts<'a>(
        composer: &'a ComposerView,
        tail_gap: bool,
        activity: &[&str],
        banner: &[&str],
    ) -> LiveParts<'a> {
        LiveParts {
            tail_gap,
            activity: activity.iter().map(|text| Row::plain(text)).collect(),
            banner: banner.iter().map(|text| Row::plain(text)).collect(),
            composer,
            hint: Row::plain("auto · m"),
        }
    }

    fn texts(layout: &LiveLayout) -> Vec<String> {
        layout.rows.iter().map(Row::text).collect()
    }

    #[test]
    fn idle_frames_place_the_composer_above_a_blank_row_and_the_status() {
        let layout = solve(parts(&prompt(), false, &[], &[]), 30);
        assert_eq!(texts(&layout), ["┃ ", "", "auto · m"]);
        assert_eq!(layout.cursor, (0, 2));
        assert_eq!(layout.footer_row, 0);
        let layout = solve(parts(&prompt(), true, &[], &[]), 30);
        assert_eq!(texts(&layout), ["", "┃ ", "", "auto · m"]);
        assert_eq!(layout.cursor, (1, 2));
        assert_eq!(layout.footer_row, 1);
    }

    #[test]
    fn activity_rows_sit_between_blank_gaps_above_the_footer() {
        let layout = solve(parts(&prompt(), true, &["• Thinking (0s)"], &[]), 30);
        assert_eq!(
            texts(&layout),
            ["", "• Thinking (0s)", "", "┃ ", "", "auto · m"]
        );
        assert_eq!(layout.cursor, (3, 2));
        assert_eq!(layout.footer_row, 1);
    }

    #[test]
    fn held_prompts_show_above_the_composer() {
        let layout = solve(parts(&prompt(), false, &["• Generating"], &["┋ next"]), 30);
        assert_eq!(
            texts(&layout),
            ["", "• Generating", "", "┋ next", "", "┃ ", "", "auto · m"]
        );
        assert_eq!(layout.cursor, (5, 2));
    }

    #[test]
    fn short_terminals_keep_the_footer_and_drop_activity_gaps() {
        let layout = solve(parts(&prompt(), true, &["• Thinking"], &[]), 4);
        assert_eq!(texts(&layout), ["• Thinking", "┃ ", "", "auto · m"]);
        assert_eq!(layout.cursor, (1, 2));
    }
}
