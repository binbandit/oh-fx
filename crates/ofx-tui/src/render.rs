use ofx_contract::PermissionMode;
use ofx_text::visible_width;

use crate::row_text::{Row, terminal_safe};
use crate::theme::Theme;

const STATUSLINE_SEPARATOR: &str = " · ";

pub(crate) fn welcome_rows(theme: &Theme, version: &str, cols: usize) -> Vec<Row> {
    let mut row = Row::styled("oh-fx", theme.subtitle);
    row.push(&format!(" v{version} · Run /help for commands"), theme.dim);
    let mut rows = row.wrapped(cols);
    rows.push(Row::new());
    rows
}

pub(crate) fn compact_model_label(model: &str) -> String {
    let bare = model.rsplit('/').next().unwrap_or(model);
    let Some(claude_name) = bare.strip_prefix("claude-") else {
        return bare.to_owned();
    };
    for (prefix, label) in [
        ("opus-", "opus "),
        ("sonnet-", "sonnet "),
        ("haiku-", "haiku "),
    ] {
        if let Some(rest) = claude_name.strip_prefix(prefix) {
            return format!("{label}{rest}");
        }
    }
    claude_name.to_owned()
}

pub(crate) fn hint_line(
    theme: &Theme,
    model: &str,
    permission_mode: PermissionMode,
    width: usize,
) -> Row {
    let model_label = compact_model_label(model);
    let permission_label = permission_mode.display_label();
    let mut row = Row::new();
    let leading_fits = width > 0
        && visible_width(permission_label)
            + visible_width(STATUSLINE_SEPARATOR)
            + visible_width(&terminal_safe(&model_label))
            <= width;
    if leading_fits {
        let paint = if permission_mode == PermissionMode::Ask {
            theme.statusline
        } else {
            theme.permission_auto
        };
        row.push(permission_label, paint);
        row.push(STATUSLINE_SEPARATOR, theme.statusline);
    }
    row.push(&model_label, theme.statusline);
    row.clipped(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_text::Paint;

    #[test]
    fn model_labels_drop_providers_and_shorten_claude_names() {
        assert_eq!(compact_model_label("openai/gpt-5"), "gpt-5");
        assert_eq!(
            compact_model_label("anthropic/claude-sonnet-4.5"),
            "sonnet 4.5"
        );
        assert_eq!(compact_model_label("claude-opus-4.7"), "opus 4.7");
        assert_eq!(compact_model_label("claude-instant"), "instant");
        assert_eq!(compact_model_label("@openai/gpt-4o"), "gpt-4o");
        assert_eq!(compact_model_label("fake-model"), "fake-model");
    }

    #[test]
    fn the_hint_line_shows_the_mode_and_model_when_both_fit() {
        let theme = Theme::builtin(false, false, true);
        let row = hint_line(&theme, "fake-model", PermissionMode::Auto, 100);
        assert_eq!(row.text(), "auto · fake-model");
        assert_eq!(row.segments()[0].paint, Paint::fg(252));
        assert_eq!(row.segments()[1].paint, Paint::fg(245));
        let ask = hint_line(&theme, "openai/gpt-5", PermissionMode::Ask, 100);
        assert_eq!(ask.text(), "ask · gpt-5");
        assert_eq!(ask.segments().len(), 1);
        assert_eq!(
            hint_line(&theme, "fake-model", PermissionMode::Auto, 12).text(),
            "fake-model"
        );
        assert_eq!(
            hint_line(&theme, "x", PermissionMode::Yolo, 40).text(),
            "full access · x"
        );
    }

    #[test]
    fn hostile_model_names_render_inert() {
        let theme = Theme::builtin(false, false, true);
        let row = hint_line(&theme, "mod\x1b]2;PWNED\x07", PermissionMode::Auto, 100);
        assert_eq!(row.text(), "auto · mod\\x1b]2;PWNED\\x07");
        assert!(!row.encode().contains('\x07'));
        assert_eq!(
            hint_line(&theme, "mod\x1b]2;PWNED\x07", PermissionMode::Auto, 25).text(),
            "mod\\x1b]2;PWNED\\x07"
        );
    }

    #[test]
    fn the_welcome_names_the_version_and_the_help_command() {
        let theme = Theme::builtin(false, false, true);
        let rows = welcome_rows(&theme, "0.1.0", 100);
        assert_eq!(rows[0].text(), "oh-fx v0.1.0 · Run /help for commands");
        assert_eq!(rows[0].segments()[0].paint, Paint::bold_fg(255));
        assert!(rows[1].text().is_empty());
        let narrow: Vec<String> = welcome_rows(&theme, "0.1.0", 20)
            .iter()
            .map(Row::text)
            .collect();
        assert_eq!(narrow, ["oh-fx v0.1.0 · Run /", "help for commands", ""]);
    }
}
