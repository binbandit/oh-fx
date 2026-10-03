use std::ops::Range;

pub(crate) const DEFAULT_MAX_PICKER_ROWS: usize = 6;

pub(crate) fn edge_from_start(count: usize, start: usize, max_rows: usize) -> Range<usize> {
    if count == 0 || max_rows == 0 {
        return 0..0;
    }
    if count <= max_rows {
        return 0..count;
    }
    let start = start.min(count - max_rows);
    start..start + max_rows
}

pub(crate) fn update_edge_start(
    current_start: usize,
    count: usize,
    selected: usize,
    max_rows: usize,
) -> usize {
    if count == 0 || max_rows == 0 || count <= max_rows {
        return 0;
    }
    let selected = selected.min(count - 1);
    let start = current_start.min(count - max_rows);
    if selected < start {
        selected
    } else if selected >= start + max_rows {
        selected + 1 - max_rows
    } else {
        start
    }
}

pub(crate) fn advance_selection(
    index: &mut usize,
    window_start: &mut usize,
    count: usize,
    delta: i32,
) {
    if count == 0 {
        return;
    }
    let current = *index % count;
    let steps = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
    *index = if delta < 0 {
        current.checked_sub(steps).unwrap_or(count - 1)
    } else {
        current
            .checked_add(steps)
            .filter(|next| *next < count)
            .unwrap_or(0)
    };
    *window_start = update_edge_start(*window_start, count, *index, DEFAULT_MAX_PICKER_ROWS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edge_window_lets_the_selection_move_back_before_scrolling() {
        let mut start = update_edge_start(0, 11, 5, 6);
        assert_eq!(start, 0);
        start = update_edge_start(start, 11, 6, 6);
        assert_eq!(start, 1);
        start = update_edge_start(start, 11, 5, 6);
        assert_eq!(start, 1);
        assert_eq!(edge_from_start(11, start, 6), 1..7);
        assert_eq!(update_edge_start(4, 11, 3, 6), 3);
        assert_eq!(update_edge_start(9, 11, 10, 6), 5);
        assert_eq!(update_edge_start(3, 4, 2, 6), 0);
        assert_eq!(edge_from_start(3, 2, 6), 0..3);
        assert_eq!(edge_from_start(11, 9, 6), 5..11);
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let (mut index, mut start) = (0, 0);
        advance_selection(&mut index, &mut start, 8, -1);
        assert_eq!((index, start), (7, 2));
        advance_selection(&mut index, &mut start, 8, 1);
        assert_eq!((index, start), (0, 0));
        advance_selection(&mut index, &mut start, 8, 3);
        assert_eq!(index, 3);
        advance_selection(&mut index, &mut start, 8, 6);
        assert_eq!(index, 0);
    }
}
