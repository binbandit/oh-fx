use ofx_contract::UiCommand;
use ofx_text::visible_width;

use super::Shell;
use crate::input::{Action, InputEvent, PasteOwner};
use crate::row_text::{Attribute, Row};

const TITLE: &str = "Sign in with Codex";
const WAITING: &str = "Waiting for authorization…";
const OPEN: &str = "  Open   ";
const AUTHORIZE: &str = "Authorize with Codex";
const LINK_ID: &str = "id=fx-codex-auth";
const HINTS: [&str; 4] = [
    "enter reopens browser · esc cancels",
    "enter reopens  esc cancels",
    "enter  esc",
    "enter esc",
];
const ROW_PRIORITY: [usize; 4] = [2, 0, 3, 1];
const FOOTER_CHROME_ROWS: usize = 5;

pub(super) struct SignInScreen {
    url: String,
}

impl Shell<'_> {
    pub(super) fn sign_in_started(&mut self, url: String) {
        self.sign_in = Some(SignInScreen { url });
    }

    pub(super) fn handle_sign_in_input(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Raw(raw) => match raw.byte {
                b'\r' | b'\n' => self.send(UiCommand::ReopenSignIn),
                3 | 4 => self.cancel_sign_in(),
                _ => {}
            },
            InputEvent::Action(decoded) => match decoded.action {
                Action::Escape => self.cancel_sign_in(),
                Action::RemappedByte(byte) => self.input.replay_byte(byte),
                Action::PasteStart => self.input.begin_paste(
                    PasteOwner::Composer,
                    crate::input::COMPOSER_INPUT_LIMIT_BYTES,
                ),
                _ => {}
            },
            InputEvent::Text(_)
            | InputEvent::Paste(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => {}
        }
    }

    fn cancel_sign_in(&mut self) {
        self.sign_in = None;
        self.send(UiCommand::CancelSignIn);
    }

    pub(super) fn sign_in_band(&self, input_extra: usize, banner_rows: usize) -> Option<Vec<Row>> {
        let screen = self.sign_in.as_ref()?;
        let width = self.cols();
        let budget = usize::from(self.layout.rows)
            .saturating_sub(FOOTER_CHROME_ROWS + input_extra + banner_rows)
            .clamp(1, ROW_PRIORITY.len());
        let mut kept: Vec<usize> = ROW_PRIORITY[..budget].to_vec();
        kept.sort_unstable();
        let mut band = vec![Row::new()];
        band.extend(kept.into_iter().map(|index| match index {
            0 => self.sign_in_title(width),
            2 => self.sign_in_action(&screen.url, width),
            _ => Row::new(),
        }));
        Some(band)
    }

    fn sign_in_title(&self, width: usize) -> Row {
        let column = (width * 2 / 3).max(22).min(width);
        let status_column = column.max(width.saturating_sub(visible_width(WAITING)));
        let mut row = Row::styled(TITLE, self.theme.dim);
        row.push_spaces(status_column.saturating_sub(row.width()));
        row.push(WAITING, self.theme.dim);
        row.clipped(width)
    }

    fn sign_in_action(&self, url: &str, width: usize) -> Row {
        let paint = self.theme.selected_completion;
        let mut row = Row::styled(OPEN, paint);
        let target = format!("{LINK_ID};{url}");
        row.push_linked(AUTHORIZE, paint.with(Attribute::Underline), Some(&target));
        row.clipped(width)
    }

    pub(super) fn sign_in_hint(&self) -> Option<Row> {
        self.sign_in.as_ref()?;
        let width = self.cols();
        let hint = HINTS
            .into_iter()
            .find(|hint| visible_width(hint) <= width)
            .unwrap_or_default();
        Some(Row::styled(hint, self.theme.statusline))
    }
}

#[cfg(test)]
mod tests;
