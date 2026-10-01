use ofx_markdown::{Attr, Slot, Style};

use crate::row_text::{Attribute, Color, Paint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    pub(crate) light: bool,
    pub(crate) hint: Paint,
    pub(crate) statusline: Paint,
    pub(crate) subtitle: Paint,
    pub(crate) tag: Paint,
    pub(crate) system_notice_label: Paint,
    pub(crate) system_notice_text: Paint,
    pub(crate) dim: Paint,
    pub(crate) warning: Paint,
    pub(crate) green: Paint,
    pub(crate) red: Paint,
    pub(crate) selected_completion: Paint,
    pub(crate) permission_auto: Paint,
    pub(crate) user_card_marker: Paint,
    diff_added_marker: Option<Color>,
    diff_removed_marker: Option<Color>,
    inline_code: Paint,
    task_completed: Paint,
    link: Paint,
    syntax_strong: Paint,
    syntax_literal: Paint,
    syntax_comment: Paint,
}

const FX_DARK: Theme = Theme {
    light: false,
    hint: Paint::fg(255),
    statusline: Paint::fg(245),
    subtitle: Paint::bold_fg(255),
    tag: Paint::bold_fg(255),
    system_notice_label: Paint::bold_fg(252),
    system_notice_text: Paint::fg(250),
    dim: Paint::fg(245),
    warning: Paint::fg(252),
    green: Paint::fg(252),
    red: Paint::fg(252),
    selected_completion: Paint::bold_fg(255),
    permission_auto: Paint::fg(252),
    user_card_marker: Paint::fg(255),
    diff_added_marker: None,
    diff_removed_marker: None,
    inline_code: Paint::fg(245),
    task_completed: Paint::fg(252),
    link: Paint::fg(75),
    syntax_strong: Paint::fg(252),
    syntax_literal: Paint::fg(250),
    syntax_comment: Paint::fg(245),
};

const FX_LIGHT: Theme = Theme {
    light: true,
    hint: Paint::fg(235),
    statusline: Paint::fg(241),
    subtitle: Paint::bold_fg(235),
    tag: Paint::bold_fg(235),
    system_notice_label: Paint::bold_fg(238),
    system_notice_text: Paint::fg(241),
    dim: Paint::fg(247),
    warning: Paint::fg(238),
    green: Paint::fg(238),
    red: Paint::fg(238),
    selected_completion: Paint::bold_fg(235),
    permission_auto: Paint::fg(238),
    user_card_marker: Paint::fg(235),
    diff_added_marker: None,
    diff_removed_marker: None,
    inline_code: Paint::fg(247),
    task_completed: Paint::fg(238),
    link: Paint::fg(25),
    syntax_strong: Paint::fg(238),
    syntax_literal: Paint::fg(241),
    syntax_comment: Paint::fg(243),
};

impl Theme {
    pub(crate) fn builtin(light: bool, pinned: bool, truecolor: bool) -> Self {
        let mut theme = if light { FX_LIGHT } else { FX_DARK };
        if pinned {
            let (added, removed) = if truecolor {
                (Color::Rgb(48, 164, 108), Color::Rgb(229, 72, 77))
            } else {
                (Color::Indexed(71), Color::Indexed(167))
            };
            theme.diff_added_marker = Some(added);
            theme.diff_removed_marker = Some(removed);
        }
        theme
    }

    pub(crate) fn accented_diff_markers(&self) -> bool {
        self.diff_added_marker.is_some()
    }

    pub(crate) fn markdown(&self, style: Style) -> Paint {
        let base = match style.slot {
            None => Paint::PLAIN,
            Some(slot) => self.slot(slot),
        };
        [
            (Attr::Bold, Attribute::Bold),
            (Attr::Dim, Attribute::Dim),
            (Attr::Italic, Attribute::Italic),
            (Attr::Underline, Attribute::Underline),
            (Attr::Strike, Attribute::Strike),
        ]
        .into_iter()
        .filter(|(attr, _)| style.has(*attr))
        .fold(base, |paint, (_, attribute)| paint.with(attribute))
    }

    fn slot(&self, slot: Slot) -> Paint {
        match slot {
            Slot::InlineCode => self.inline_code,
            Slot::Link => self.link,
            Slot::TaskCompleted => self.task_completed,
            Slot::Dim | Slot::SyntaxComment => self.syntax_comment_or_dim(slot),
            Slot::DiffAddedMarker => Paint::colored(self.diff_added_marker),
            Slot::DiffRemovedMarker => Paint::colored(self.diff_removed_marker),
            Slot::SyntaxKeyword
            | Slot::SyntaxFunction
            | Slot::SyntaxVariable
            | Slot::SyntaxOperator => self.syntax_strong,
            Slot::SyntaxString | Slot::SyntaxNumber => self.syntax_literal,
        }
    }

    fn syntax_comment_or_dim(&self, slot: Slot) -> Paint {
        if slot == Slot::Dim {
            self.dim
        } else {
            self.syntax_comment
        }
    }
}

#[cfg(test)]
mod tests {
    use ofx_markdown::{Completions, Event, MarkdownProcessor, Span, highlight, resolve};

    use super::*;

    fn paints(theme: &Theme) -> [Paint; 18] {
        [
            theme.hint,
            theme.statusline,
            theme.subtitle,
            theme.system_notice_label,
            theme.system_notice_text,
            theme.dim,
            theme.warning,
            theme.green,
            theme.red,
            theme.selected_completion,
            theme.permission_auto,
            theme.user_card_marker,
            theme.inline_code,
            theme.task_completed,
            theme.link,
            theme.syntax_strong,
            theme.syntax_literal,
            theme.syntax_comment,
        ]
    }

    fn rendered_spans(markdown: &str) -> Vec<Span> {
        let mut processor = MarkdownProcessor::with_completions(Completions::ALL);
        let mut events = Vec::new();
        processor.push(markdown, &mut events);
        processor.flush(&mut events);
        events
            .into_iter()
            .filter_map(|event| match event {
                Event::Line(line) => Some(line.spans),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn span_style(spans: &[Span], text: &str) -> Style {
        spans
            .iter()
            .find(|span| span.text == text)
            .map(|span| span.style)
            .unwrap()
    }

    #[test]
    fn builtin_selects_the_variant_matching_the_light_flag() {
        assert!(!Theme::builtin(false, false, true).light);
        assert!(Theme::builtin(true, false, true).light);
        assert_eq!(Theme::builtin(false, false, false), FX_DARK);
        assert_eq!(Theme::builtin(true, false, false), FX_LIGHT);
    }

    #[test]
    fn every_theme_slot_is_populated() {
        for theme in [FX_DARK, FX_LIGHT] {
            assert!(paints(&theme).iter().all(|paint| paint.fg.is_some()));
        }
        for light in [false, true] {
            for truecolor in [false, true] {
                let pinned = Theme::builtin(light, true, truecolor);
                assert!(pinned.diff_added_marker.is_some());
                assert!(pinned.diff_removed_marker.is_some());
            }
        }
    }

    #[test]
    fn builtin_themes_pin_the_historical_fx_palette_bytes() {
        assert_eq!(FX_DARK.hint, Paint::fg(255));
        assert_eq!(FX_DARK.statusline, Paint::fg(245));
        assert_eq!(FX_DARK.user_card_marker, Paint::fg(255));
        assert_eq!(FX_DARK.inline_code, Paint::fg(245));
        assert_eq!(FX_DARK.task_completed, Paint::fg(252));
        assert_eq!(FX_DARK.syntax_strong, Paint::fg(252));
        assert_eq!(FX_DARK.syntax_comment, Paint::fg(245));
        assert_eq!(FX_LIGHT.hint, Paint::fg(235));
        assert_eq!(FX_LIGHT.statusline, Paint::fg(241));
        assert_eq!(FX_LIGHT.user_card_marker, Paint::fg(235));
        assert_eq!(FX_LIGHT.inline_code, Paint::fg(247));
        assert_eq!(FX_LIGHT.task_completed, Paint::fg(238));
        assert_eq!(FX_LIGHT.syntax_strong, Paint::fg(238));
        assert_eq!(FX_LIGHT.syntax_comment, Paint::fg(243));
        for light in [false, true] {
            let truecolor = Theme::builtin(light, true, true);
            assert_eq!(truecolor.diff_added_marker, Some(Color::Rgb(48, 164, 108)));
            assert_eq!(truecolor.diff_removed_marker, Some(Color::Rgb(229, 72, 77)));
            let fallback = Theme::builtin(light, true, false);
            assert_eq!(fallback.diff_added_marker, Some(Color::Indexed(71)));
            assert_eq!(fallback.diff_removed_marker, Some(Color::Indexed(167)));
        }
    }

    #[test]
    fn builtin_palettes_follow_the_upstream_tokens() {
        let dark = Theme::builtin(false, false, true);
        assert_eq!(dark.hint, Paint::fg(255));
        assert_eq!(dark.statusline, Paint::fg(245));
        assert_eq!(dark.subtitle, Paint::bold_fg(255));
        assert_eq!(dark.permission_auto, Paint::fg(252));
        let light = Theme::builtin(true, false, true);
        assert_eq!(light.hint, Paint::fg(235));
        assert_eq!(light.dim, Paint::fg(247));
        assert!(light.light);
    }

    #[test]
    fn diff_markers_are_colored_only_for_pinned_themes() {
        let following = Theme::builtin(false, false, true);
        assert_eq!(following.slot(Slot::DiffAddedMarker), Paint::PLAIN);
        let pinned = Theme::builtin(false, true, true);
        assert_eq!(
            pinned.slot(Slot::DiffAddedMarker).fg,
            Some(Color::Rgb(48, 164, 108))
        );
        let palette = Theme::builtin(false, true, false);
        assert_eq!(
            palette.slot(Slot::DiffRemovedMarker).fg,
            Some(Color::Indexed(167))
        );
    }

    #[test]
    fn markdown_styles_combine_attributes_with_theme_slots() {
        let theme = Theme::builtin(false, false, true);
        let spans = rendered_spans("`code` and [docs](https://example.com) and **bold**\n");
        assert_eq!(theme.markdown(span_style(&spans, "code")), Paint::fg(245));
        let painted = theme.markdown(span_style(&spans, "docs"));
        assert!(painted.has(Attribute::Underline));
        assert_eq!(painted.fg, Some(Color::Indexed(75)));
        let painted = theme.markdown(span_style(&spans, "bold"));
        assert!(painted.has(Attribute::Bold) && painted.fg.is_none());
        let rust = resolve("rust").unwrap();
        let keyword = highlight("fn main() {}", rust, None)[0].spans[0].style;
        assert_eq!(keyword.slot, Some(Slot::SyntaxKeyword));
        assert_eq!(theme.markdown(keyword), Paint::fg(252));
    }
}
