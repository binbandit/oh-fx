use ofx_contract::{PermissionMode, WorkspaceIdentity};
use ofx_text::{prefix_by_width, visible_width};

use crate::footer::statusline::{
    MAX_STATUS_LINE_BYTES, StatuslineView, context_segment, workspace_identity_segment,
};
use crate::row_text::{Row, terminal_safe};
use crate::theme::Theme;

const STATUSLINE_SEPARATOR: &str = " · ";
const MAX_SESSION_TITLE_CELLS: usize = 32;

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
    statusline: StatuslineView<'_>,
    width: usize,
) -> Row {
    let model_label = compact_model_label(model);
    let permission_label = permission_mode.display_label();
    let mut row = Row::new();
    let status_limit = width.min(MAX_STATUS_LINE_BYTES);
    let leading_fits = status_limit > 0
        && visible_width(permission_label)
            + visible_width(STATUSLINE_SEPARATOR)
            + visible_width(&terminal_safe(&model_label))
            <= status_limit;
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
    if let Some(title) = statusline.session_title {
        push_segment(&mut row, theme, &session_title_segment(title));
    }
    if let Some(context) = context_segment(statusline.context_used, statusline.context_total) {
        push_segment(&mut row, theme, &context);
    }
    if let Some(identity) = statusline.identity {
        push_workspace_identity(&mut row, theme, identity, status_limit);
    }
    row.clipped(width)
}

fn session_title_segment(title: &str) -> String {
    let visible: String = title
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    prefix_by_width(&visible, MAX_SESSION_TITLE_CELLS).to_owned()
}

fn push_segment(row: &mut Row, theme: &Theme, segment: &str) {
    if segment.is_empty() {
        return;
    }
    if row.width() > 0 {
        row.push(STATUSLINE_SEPARATOR, theme.statusline);
    }
    row.push(segment, theme.statusline);
}

fn push_workspace_identity(
    row: &mut Row,
    theme: &Theme,
    identity: &WorkspaceIdentity,
    status_limit: usize,
) {
    let used_width = row.width();
    let separator_width = if used_width > 0 {
        visible_width(STATUSLINE_SEPARATOR)
    } else {
        0
    };
    if used_width + separator_width >= status_limit {
        return;
    }
    let used_bytes = row.byte_len()
        + if used_width > 0 {
            STATUSLINE_SEPARATOR.len()
        } else {
            0
        };
    let Some(available_bytes) = MAX_STATUS_LINE_BYTES.checked_sub(used_bytes) else {
        return;
    };
    if let Some(segment) = workspace_identity_segment(
        identity,
        status_limit - used_width - separator_width,
        available_bytes,
    ) {
        push_segment(row, theme, &segment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_text::Paint;

    fn line(
        model: &str,
        mode: PermissionMode,
        statusline: StatuslineView<'_>,
        width: usize,
    ) -> String {
        hint_line(
            &Theme::builtin(false, false, true),
            model,
            mode,
            statusline,
            width,
        )
        .text()
    }

    fn identity(label: &str, branch: Option<&str>) -> WorkspaceIdentity {
        WorkspaceIdentity {
            label: label.to_owned(),
            branch: branch.map(str::to_owned),
        }
    }

    fn workspace(identity: &WorkspaceIdentity) -> StatuslineView<'_> {
        StatuslineView {
            identity: Some(identity),
            ..StatuslineView::default()
        }
    }

    #[test]
    fn the_hint_line_shows_context_usage_with_and_without_a_known_total() {
        let full = StatuslineView {
            context_used: 43_000,
            context_total: Some(1_000_000),
            session_title: None,
            identity: None,
        };
        assert_eq!(
            line("anthropic/claude-opus-4.8", PermissionMode::Ask, full, 80),
            "ask · opus 4.8 · 43k/1000k 4%"
        );
        let open = StatuslineView {
            context_used: 163_000,
            ..StatuslineView::default()
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, open, 80),
            "ask · gpt-5 · 163k"
        );
        let unused = StatuslineView {
            context_total: Some(1_000_000),
            ..StatuslineView::default()
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, unused, 80),
            "ask · gpt-5"
        );
        let empty_window = StatuslineView {
            context_used: 1_500,
            context_total: Some(0),
            session_title: None,
            identity: None,
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, empty_window, 80),
            "ask · gpt-5 · 1k/0k 0%"
        );
    }

    #[test]
    fn the_hint_line_shows_the_session_title_after_the_model() {
        let branch = identity("/tmp/fx", Some("main"));
        let titled = StatuslineView {
            context_used: 12_000,
            session_title: Some("Fix the renderer"),
            ..workspace(&branch)
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, titled, 100),
            "ask · gpt-5 · Fix the renderer · 12k · /tmp/fx (main)"
        );
        let long = StatuslineView {
            session_title: Some("Investigate the flaky renderer resize test on macOS"),
            ..StatuslineView::default()
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, long, 100),
            "ask · gpt-5 · Investigate the flaky renderer r"
        );
        let wide = StatuslineView {
            session_title: Some("界面渲染错误修复与终端尺寸变化测试稳定"),
            ..StatuslineView::default()
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, wide, 100),
            "ask · gpt-5 · 界面渲染错误修复与终端尺寸变化测"
        );
        let controls = StatuslineView {
            session_title: Some("Fix\u{9b}31m it"),
            ..StatuslineView::default()
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, controls, 100),
            "ask · gpt-5 · Fix31m it"
        );
        let empty = StatuslineView {
            session_title: Some(""),
            ..StatuslineView::default()
        };
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, empty, 100),
            "ask · gpt-5"
        );
    }

    #[test]
    fn the_hint_line_shows_the_workspace_and_git_branch() {
        let branch = identity("/workspace/code/fx", Some("feature/statusline"));
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, workspace(&branch), 100),
            "ask · gpt-5 · /workspace/code/fx (feature/statusline)"
        );
        let plain = identity("/tmp/plain-workspace", None);
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, workspace(&plain), 80),
            "ask · gpt-5 · /tmp/plain-workspace"
        );
        let detached = identity("/tmp/fx", Some("detached:0123456789ab"));
        assert_eq!(
            line(
                "openai/gpt-5",
                PermissionMode::Ask,
                workspace(&detached),
                80
            ),
            "ask · gpt-5 · /tmp/fx (detached:0123456789ab)"
        );
        let missing = identity("", Some("main"));
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, workspace(&missing), 80),
            "ask · gpt-5"
        );
    }

    #[test]
    fn the_workspace_and_branch_stay_readable_at_narrow_widths() {
        let long = identity("/a/very/long/path/to/fx-repo", Some("feature/statusline"));
        let text = line("openai/gpt-5", PermissionMode::Ask, workspace(&long), 36);
        assert_eq!(visible_width(&text), 36);
        assert!(text.starts_with("ask · gpt-5 · "), "{text}");
        assert!(text.contains("fx-repo"), "{text}");
        assert!(text.contains("feature/"), "{text}");
        assert!(text.ends_with("…)"), "{text}");
        assert_eq!(text, "ask · gpt-5 · …fx-repo (feature/st…)");
        let narrow = line("openai/gpt-5", PermissionMode::Ask, workspace(&long), 20);
        assert_eq!(narrow, "ask · gpt-5 · …-repo");
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, workspace(&long), 15),
            "ask · gpt-5 · …"
        );
        assert_eq!(
            line("openai/gpt-5", PermissionMode::Ask, workspace(&long), 14),
            "ask · gpt-5"
        );
    }

    #[test]
    fn the_workspace_identity_follows_the_context_segment() {
        let long = identity(
            "/a/very/long/path/to/the/active/workspace",
            Some("feature/statusline"),
        );
        let statusline = StatuslineView {
            context_used: 1_000,
            context_total: Some(100_000),
            session_title: None,
            identity: Some(&long),
        };
        let text = line(
            "anthropic/claude-opus-4.8",
            PermissionMode::Auto,
            statusline,
            60,
        );
        assert!(
            text.starts_with("auto · opus 4.8 · 1k/100k 1% · "),
            "{text}"
        );
        assert_eq!(visible_width(&text), 60);
        assert!(text.ends_with("…)"), "{text}");
    }

    #[test]
    fn escaped_workspace_labels_are_clipped_on_escape_boundaries() {
        let label = identity("/work/\\x1b\\x1b", None);
        assert_eq!(
            line("m", PermissionMode::Ask, workspace(&label), 17),
            "ask · m · …\\x1b"
        );
        let branch = identity("/w", Some("\\x1b\\x1bmain"));
        assert_eq!(
            line("m", PermissionMode::Ask, workspace(&branch), 24),
            "ask · m · /w (\\x1b…)"
        );
        assert_eq!(
            line("m", PermissionMode::Ask, workspace(&branch), 30),
            "ask · m · /w (\\x1b\\x1bm…)"
        );
    }

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
        let row = hint_line(
            &theme,
            "fake-model",
            PermissionMode::Auto,
            StatuslineView::default(),
            100,
        );
        assert_eq!(row.text(), "auto · fake-model");
        assert_eq!(row.segments()[0].paint, Paint::fg(252));
        assert_eq!(row.segments()[1].paint, Paint::fg(245));
        let ask = hint_line(
            &theme,
            "openai/gpt-5",
            PermissionMode::Ask,
            StatuslineView::default(),
            100,
        );
        assert_eq!(ask.text(), "ask · gpt-5");
        assert_eq!(ask.segments().len(), 1);
        assert_eq!(
            hint_line(
                &theme,
                "fake-model",
                PermissionMode::Auto,
                StatuslineView::default(),
                12
            )
            .text(),
            "fake-model"
        );
        assert_eq!(
            hint_line(
                &theme,
                "x",
                PermissionMode::Yolo,
                StatuslineView::default(),
                40
            )
            .text(),
            "full access · x"
        );
    }

    #[test]
    fn hostile_model_names_render_inert() {
        let theme = Theme::builtin(false, false, true);
        let row = hint_line(
            &theme,
            "mod\x1b]2;PWNED\x07",
            PermissionMode::Auto,
            StatuslineView::default(),
            100,
        );
        assert_eq!(row.text(), "auto · mod\\x1b]2;PWNED\\x07");
        assert!(!row.encode().contains('\x07'));
        assert_eq!(
            hint_line(
                &theme,
                "mod\x1b]2;PWNED\x07",
                PermissionMode::Auto,
                StatuslineView::default(),
                25
            )
            .text(),
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
