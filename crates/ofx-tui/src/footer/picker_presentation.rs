use ofx_text::{prefix_by_width, visible_width};

use crate::composer::file_completion_state::{FileMatch, MentionKind};
use crate::list_window::{DEFAULT_MAX_PICKER_ROWS, edge_from_start, update_edge_start};
use crate::row_text::{EllipsisPlacement, Paint, Row, display_safe_suffix};
use crate::theme::Theme;

const FIXED_FOOTER_ROWS: usize = 5;
const MINIMUM_TRANSCRIPT_ROWS: usize = 5;
const MINIMUM_SEGMENTED_WIDTH: usize = 8;
const ELLIPSIS: &str = "\u{2026}";
const CTRL_C_EXIT_HINT: &str = "press ctrl+c again to exit";
const ANNOTATION_SEPARATOR: &str = " · ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilePickerStatus<'a> {
    Rows,
    Loading,
    Empty,
    Notice(&'a str),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FilePickerFrame<'a> {
    pub(crate) items: &'a [FileMatch],
    pub(crate) selected: Option<usize>,
    pub(crate) window_start: usize,
    pub(crate) status: FilePickerStatus<'a>,
    pub(crate) start_col: usize,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
}

pub(crate) fn list_picker_rows(
    terminal_rows: usize,
    input_extra: usize,
    banner_rows: usize,
) -> usize {
    let available = terminal_rows.saturating_sub(FIXED_FOOTER_ROWS + input_extra + banner_rows);
    if available == 0 {
        1
    } else {
        available.min(DEFAULT_MAX_PICKER_ROWS)
    }
}

pub(crate) fn menu_row_budget(
    terminal_rows: usize,
    input_extra: usize,
    banner_rows: usize,
    max_rows: usize,
) -> usize {
    let available = terminal_rows
        .saturating_sub(FIXED_FOOTER_ROWS + input_extra + banner_rows)
        .saturating_sub(MINIMUM_TRANSCRIPT_ROWS);
    max_rows.min(available.max(1))
}

pub(crate) fn catalog_menu_hint_row(
    theme: &Theme,
    width: usize,
    ctrl_c_pending: bool,
    tab: &str,
) -> Row {
    let hints = [
        format!("↑↓ navigate     tab {tab}     enter use     esc close"),
        format!("↑↓ navigate  tab {tab}  enter use  esc close"),
        format!("↑↓ move  tab {tab}  enter  esc"),
        "enter use  esc close".to_owned(),
        "enter esc".to_owned(),
    ];
    menu_hint_row(
        theme,
        width,
        ctrl_c_pending,
        &hints.each_ref().map(String::as_str),
    )
}

pub(crate) fn menu_hint_row(
    theme: &Theme,
    width: usize,
    ctrl_c_pending: bool,
    hints: &[&str],
) -> Row {
    if ctrl_c_pending {
        return Row::styled(CTRL_C_EXIT_HINT, theme.statusline).clipped(width);
    }
    let hint = hints
        .iter()
        .find(|hint| visible_width(hint) <= width)
        .or(hints.last())
        .copied()
        .unwrap_or_default();
    Row::styled(hint, theme.dim).clipped(width)
}

pub(crate) fn inline_menu_band(rows: Vec<Row>) -> Vec<Row> {
    if rows.is_empty() {
        return rows;
    }
    let mut band = Vec::with_capacity(rows.len() + 2);
    band.push(Row::new());
    band.extend(rows);
    band.push(Row::new());
    band
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OptionList<'a> {
    pub(crate) labels: &'a [String],
    pub(crate) annotations: &'a [&'a str],
    pub(crate) cursor: (usize, usize),
    pub(crate) empty: &'a str,
    pub(crate) start_col: usize,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
}

pub(crate) fn option_picker_band(theme: &Theme, list: &OptionList<'_>) -> Vec<Row> {
    let mut band = vec![picker_divider(theme, list.cols)];
    if list.labels.is_empty() {
        band.push(picker_status_row(
            theme,
            list.start_col,
            list.empty,
            list.cols,
        ));
    } else {
        let count = list.labels.len();
        let selected = list.cursor.0 % count;
        let start = update_edge_start(list.cursor.1, count, selected, list.rows);
        for index in edge_from_start(count, start, list.rows) {
            let annotation = list.annotations.get(index).copied().unwrap_or_default();
            band.push(option_row(
                theme,
                list,
                index,
                annotation,
                index == selected,
            ));
        }
    }
    band.resize_with(list.rows.max(1) + 1, Row::new);
    band.push(picker_divider(theme, list.cols));
    band
}

fn option_row(
    theme: &Theme,
    list: &OptionList<'_>,
    index: usize,
    annotation: &str,
    selected: bool,
) -> Row {
    let Some((mut row, width)) = picker_row_at(list.start_col, list.cols) else {
        return Row::new();
    };
    let label = &list.labels[index];
    let annotation_width = if annotation.is_empty() {
        0
    } else {
        visible_width(ANNOTATION_SEPARATOR) + visible_width(annotation)
    };
    let show_annotation = annotation_width > 0 && width >= visible_width(label) + annotation_width;
    let label_width = width - if show_annotation { annotation_width } else { 0 };
    let paint = if selected {
        theme.selected_completion
    } else {
        theme.dim
    };
    row.push(prefix_by_width(label, label_width), paint);
    if show_annotation {
        row.push(ANNOTATION_SEPARATOR, theme.dim);
        row.push(annotation, theme.dim);
    }
    row
}

pub(crate) fn file_picker_band(theme: &Theme, frame: &FilePickerFrame<'_>) -> Vec<Row> {
    let mut band = vec![picker_divider(theme, frame.cols)];
    band.extend(file_picker_rows(theme, frame));
    band.push(picker_divider(theme, frame.cols));
    band
}

pub(crate) fn picker_divider(theme: &Theme, cols: usize) -> Row {
    Row::styled(&"\u{2500}".repeat(cols), theme.divider)
}

fn file_picker_rows(theme: &Theme, frame: &FilePickerFrame<'_>) -> Vec<Row> {
    let mut rows = Vec::with_capacity(frame.rows);
    let notice = match frame.status {
        FilePickerStatus::Notice(text) => Some(text),
        _ => None,
    };
    if frame.items.is_empty() {
        let label = match frame.status {
            FilePickerStatus::Notice(text) => text,
            FilePickerStatus::Loading => "indexing files...",
            FilePickerStatus::Rows | FilePickerStatus::Empty => "no matching files",
        };
        rows.push(picker_status_row(theme, frame.start_col, label, frame.cols));
    } else {
        let count = frame.items.len();
        let selected = frame.selected.unwrap_or(0) % count;
        if let Some(text) = notice.filter(|_| frame.rows > 1) {
            rows.push(picker_status_row(theme, frame.start_col, text, frame.cols));
        }
        let visible = frame.rows.saturating_sub(rows.len());
        let start = update_edge_start(frame.window_start, count, selected, visible);
        let window = edge_from_start(count, start, visible);
        for (index, item) in frame
            .items
            .iter()
            .enumerate()
            .skip(window.start)
            .take(window.len())
        {
            let highlighted = frame.selected.is_some() && index == selected;
            rows.push(file_row(
                theme,
                frame.start_col,
                item,
                highlighted,
                frame.cols,
            ));
        }
    }
    rows.resize_with(frame.rows.max(rows.len()), Row::new);
    rows.truncate(frame.rows);
    rows
}

pub(crate) fn picker_row_at(start_col: usize, cols: usize) -> Option<(Row, usize)> {
    if cols == 0 || start_col == 0 || start_col > cols {
        return None;
    }
    let mut row = Row::new();
    row.push_spaces(start_col - 1);
    Some((row, cols - (start_col - 1)))
}

pub(crate) fn picker_status_row(theme: &Theme, start_col: usize, text: &str, cols: usize) -> Row {
    let Some((mut row, width)) = picker_row_at(start_col, cols) else {
        return Row::new();
    };
    row.push(prefix_by_width(text, width), theme.dim);
    row
}

fn file_row(theme: &Theme, start_col: usize, item: &FileMatch, selected: bool, cols: usize) -> Row {
    let Some((mut row, width)) = picker_row_at(start_col, cols) else {
        return Row::new();
    };
    let base = if selected {
        theme.picker_selected
    } else {
        theme.dim
    };
    FileLabel {
        row: &mut row,
        item,
        base,
    }
    .append(width);
    row
}

struct FileLabel<'a> {
    row: &'a mut Row,
    item: &'a FileMatch,
    base: Paint,
}

impl FileLabel<'_> {
    fn append(&mut self, width: usize) {
        let path = self.item.path.as_str();
        let directory = self.item.kind == MentionKind::Directory;
        let slash_width = usize::from(directory);
        if visible_width(path) + slash_width <= width {
            self.styled(0, path.len());
            if directory {
                self.row.push("/", self.base);
            }
            return;
        }
        let basename_start = path.rfind('/').map_or(0, |slash| slash + 1);
        let Some(slash) = path.rfind('/') else {
            self.basename_projection(basename_start, width);
            return;
        };
        if width < MINIMUM_SEGMENTED_WIDTH {
            self.basename_projection(basename_start, width);
            return;
        }
        let dirname_len = if slash == 0 { 1 } else { slash };
        let dirname = path.get(..dirname_len).unwrap_or_default();
        let directory_budget = visible_width(dirname).min((width / 3).clamp(3, 12));
        let basename_budget = width - directory_budget - 1 - slash_width;
        self.ellipsized(0, dirname_len, directory_budget, EllipsisPlacement::Middle);
        self.styled(dirname_len, basename_start);
        self.ellipsized(
            basename_start,
            path.len(),
            basename_budget,
            EllipsisPlacement::PrefixBiased,
        );
        if directory {
            self.row.push("/", self.base);
        }
    }

    fn basename_projection(&mut self, basename_start: usize, width: usize) {
        let end = self.item.path.len();
        if self.item.kind == MentionKind::Directory {
            if width == 0 {
                return;
            }
            if width > 1 {
                self.ellipsized(
                    basename_start,
                    end,
                    width - 1,
                    EllipsisPlacement::PrefixBiased,
                );
            }
            self.row.push("/", self.base);
            return;
        }
        self.ellipsized(basename_start, end, width, EllipsisPlacement::PrefixBiased);
    }

    fn ellipsized(&mut self, start: usize, end: usize, width: usize, placement: EllipsisPlacement) {
        if width == 0 || start >= end {
            return;
        }
        let source = self.item.path.get(start..end).unwrap_or_default();
        if visible_width(source) <= width {
            self.styled(start, end);
            return;
        }
        if width == 1 {
            self.row.push(ELLIPSIS, self.base);
            return;
        }
        let content = width - 1;
        let (prefix_width, suffix_width) = placement.split(content);
        let prefix = prefix_by_width(source, prefix_width);
        let suffix = display_safe_suffix(source, suffix_width);
        self.styled(start, start + prefix.len());
        self.row.push(ELLIPSIS, self.base);
        self.styled(end - suffix.len(), end);
    }

    fn styled(&mut self, start: usize, end: usize) {
        let path = self.item.path.as_str();
        let mut cursor = start;
        for span in &self.item.spans {
            if span.end <= start {
                continue;
            }
            if span.start >= end {
                break;
            }
            let visible_start = span.start.max(start);
            let visible_end = span.end.min(end);
            if cursor < visible_start {
                self.row.push(
                    path.get(cursor..visible_start).unwrap_or_default(),
                    self.base,
                );
            }
            self.row.push(
                path.get(visible_start..visible_end).unwrap_or_default(),
                self.base.with_bold(),
            );
            cursor = visible_end;
        }
        if cursor < end {
            self.row
                .push(path.get(cursor..end).unwrap_or_default(), self.base);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;

    fn theme() -> Theme {
        Theme::builtin(false, true, true)
    }

    fn single(range: Range<usize>) -> Vec<Range<usize>> {
        std::iter::once(range).collect()
    }

    fn item(path: &str, kind: MentionKind, spans: Vec<Range<usize>>) -> FileMatch {
        FileMatch {
            path: path.to_owned(),
            kind,
            spans,
        }
    }

    fn label(path: &str, kind: MentionKind, width: usize) -> String {
        let mut row = Row::new();
        FileLabel {
            row: &mut row,
            item: &item(path, kind, Vec::new()),
            base: Paint::PLAIN,
        }
        .append(width);
        row.text()
    }

    #[test]
    fn narrow_rows_keep_distinguishing_basename_text() {
        let prefix = "deeply-nested-source-directory/";
        for (number, selected) in [(15, true), (16, false)] {
            let path =
                format!("{prefix}alpha-component-{number}-with-a-very-long-descriptive-name.zig");
            let row = file_row(
                &theme(),
                3,
                &item(&path, MentionKind::File, Vec::new()),
                selected,
                40,
            );
            let text = row.text();
            assert!(
                text.contains(&format!("alpha-component-{number}")),
                "{text}"
            );
            assert!(!text.contains(prefix), "{text}");
            assert!(visible_width(&text) <= 40);
        }
    }

    #[test]
    fn duplicate_basenames_keep_their_directory_identity_and_tiny_rows_their_basename() {
        let basename = "shared-component-id-with-a-very-long-name.zig";
        assert_eq!(
            label(
                &format!("first-distinguishing-parent-with-long-name/{basename}"),
                MentionKind::File,
                38
            ),
            "first-\u{2026}-name/shared-component-i\u{2026}me.zig"
        );
        assert_eq!(
            label(
                &format!("second-distinguishing-parent-with-long-name/{basename}"),
                MentionKind::File,
                38
            ),
            "second\u{2026}-name/shared-component-i\u{2026}me.zig"
        );
        assert_eq!(
            label(
                "first-distinguishing-parent/shared-component.zig",
                MentionKind::File,
                4
            ),
            "sha\u{2026}"
        );
    }

    #[test]
    fn directories_end_with_a_slash_and_spans_render_bold_over_the_base() {
        let directory = item("src/main", MentionKind::Directory, vec![4..5, 6..7]);
        let unselected = file_row(&theme(), 1, &directory, false, 40);
        assert_eq!(unselected.text(), "src/main/");
        let encoded = unselected.encode();
        assert!(
            encoded.contains("\u{1b}[0;38;5;245msrc/\u{1b}[0;1;38;5;245mm\u{1b}[0;38;5;245ma\u{1b}[0;1;38;5;245mi\u{1b}[0;38;5;245mn/"),
            "{encoded:?}"
        );
        let selected = file_row(&theme(), 1, &directory, true, 40).encode();
        assert!(
            selected.contains("\u{1b}[0;1;48;5;239;38;5;255mm"),
            "{selected:?}"
        );
        let tiny = file_row(
            &theme(),
            1,
            &item("deeply/nested", MentionKind::Directory, Vec::new()),
            false,
            1,
        );
        assert_eq!(tiny.text(), "/");
    }

    #[test]
    fn clipping_keeps_spans_on_retained_segments_and_combining_marks_whole() {
        let path = "first-distinguishing-parent/target-component-with-long-name.zig";
        let target = path.find("target").unwrap();
        let parent = file_row(
            &theme(),
            1,
            &item(path, MentionKind::File, single(0..1)),
            false,
            38,
        );
        assert!(parent.encode().contains("\u{1b}[0;1;38;5;245mf"));
        let basename = file_row(
            &theme(),
            1,
            &item(path, MentionKind::File, single(target..target + 1)),
            false,
            38,
        );
        assert!(basename.encode().contains("\u{1b}[0;1;38;5;245mt"));
        assert!(visible_width(&basename.text()) <= 38);
        assert_eq!(
            label("zzabcd\u{301}e", MentionKind::File, 6),
            "zzab\u{2026}e"
        );
        assert_eq!(
            label("abcd\u{301}e/target", MentionKind::Directory, 8),
            "a\u{2026}e/ta\u{2026}/"
        );
        let cafe = "docs/Cafe\u{301}.txt";
        let cluster = cafe.find('e').unwrap();
        let highlighted = file_row(
            &theme(),
            1,
            &item(cafe, MentionKind::File, single(cluster..cluster + 3)),
            false,
            40,
        );
        assert!(
            highlighted
                .encode()
                .contains("\u{1b}[0;1;38;5;245me\u{301}")
        );
        assert_eq!(visible_width(&highlighted.text()), visible_width(cafe));
    }

    #[test]
    fn the_band_shows_status_rows_and_scrolls_the_selection_into_view() {
        let items: Vec<FileMatch> = (0..10)
            .map(|index| item(&format!("file-{index}.txt"), MentionKind::File, Vec::new()))
            .collect();
        let frame = FilePickerFrame {
            items: &items,
            selected: Some(8),
            window_start: 0,
            status: FilePickerStatus::Rows,
            start_col: 3,
            cols: 30,
            rows: 6,
        };
        let band: Vec<String> = file_picker_band(&theme(), &frame)
            .iter()
            .map(Row::text)
            .collect();
        assert_eq!(band.len(), 8);
        assert_eq!(band[0], "\u{2500}".repeat(30));
        assert_eq!(band[1], "  file-3.txt");
        assert_eq!(band[6], "  file-8.txt");
        let loading = FilePickerFrame {
            items: &[],
            status: FilePickerStatus::Loading,
            ..frame
        };
        let rows: Vec<String> = file_picker_rows(&theme(), &loading)
            .iter()
            .map(Row::text)
            .collect();
        assert_eq!(rows, ["  indexing files...", "", "", "", "", ""]);
        let stale = FilePickerFrame {
            selected: None,
            status: FilePickerStatus::Notice(
                "Selection unavailable. navigate to choose; tab to retry.",
            ),
            ..frame
        };
        let rows: Vec<String> = file_picker_rows(&theme(), &stale)
            .iter()
            .map(Row::text)
            .collect();
        assert!(rows[0].starts_with("  Selection unavailable."));
        assert_eq!(rows[1], "  file-0.txt");
        assert_eq!(list_picker_rows(24, 0, 0), 6);
        assert_eq!(list_picker_rows(8, 0, 0), 3);
        assert_eq!(list_picker_rows(5, 1, 0), 1);
    }

    #[test]
    fn inline_menu_bands_put_a_blank_row_on_each_side_of_their_rows() {
        assert!(inline_menu_band(Vec::new()).is_empty());
        let band = inline_menu_band(vec![Row::plain("menu")]);
        assert_eq!(
            band.iter().map(Row::text).collect::<Vec<_>>(),
            ["", "menu", ""]
        );
    }
}
