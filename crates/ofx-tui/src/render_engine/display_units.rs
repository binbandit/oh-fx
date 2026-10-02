use ofx_text::display_unit_at;

use crate::row_text::Paint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unit<'a> {
    pub(crate) text: &'a str,
    pub(crate) width: usize,
    pub(crate) paint: Paint,
    pub(crate) link: Option<&'a str>,
}

impl Unit<'_> {
    pub(crate) fn is_space(&self) -> bool {
        self.text == " "
    }
}

pub(crate) fn display_units<'a>(
    text: &'a str,
    paint: Paint,
    link: Option<&'a str>,
) -> impl Iterator<Item = Unit<'a>> {
    let mut index = 0;
    std::iter::from_fn(move || {
        if index >= text.len() {
            return None;
        }
        let unit = display_unit_at(text, index);
        let end = index + unit.byte_len.max(1);
        let item = Unit {
            text: &text[index..end],
            width: unit.cell_width,
            paint,
            link,
        };
        index = end;
        Some(item)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_borrow_whole_display_units_with_their_style() {
        let units: Vec<Unit<'_>> = display_units("a界 ", Paint::fg(1), Some("t")).collect();
        let texts: Vec<&str> = units.iter().map(|unit| unit.text).collect();
        assert_eq!(texts, ["a", "界", " "]);
        assert_eq!(units[1].width, 2);
        assert!(units[2].is_space());
        assert!(units.iter().all(|unit| unit.link == Some("t")));
    }
}
