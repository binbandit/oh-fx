use super::input_action::{Action, MoveIntent, MoveKind, ShortcutAction};

fn move_to(kind: MoveKind) -> ShortcutAction {
    ShortcutAction::Move(MoveIntent::new(kind))
}

impl ShortcutAction {
    pub(crate) fn from_control_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => move_to(MoveKind::LineStart),
            5 => move_to(MoveKind::LineEnd),
            2 => move_to(MoveKind::CharacterLeft),
            6 => move_to(MoveKind::CharacterRight),
            14 => Self::HistoryNext,
            4 => Self::DeleteForward,
            11 => Self::DeleteToLineEnd,
            21 => Self::DeleteToLineStart,
            23 => Self::DeleteWhitespaceWordLeft,
            25 => Self::Yank,
            31 => Self::Undo,
            12 => Self::Redraw,
            127 | 8 => Self::DeleteBackward,
            b'\n' => Self::InsertNewline,
            _ => return None,
        })
    }

    pub(crate) fn from_escape_action(action: Action) -> Option<Self> {
        Some(match action {
            Action::CursorUp => move_to(MoveKind::VisualUp),
            Action::CursorDown => move_to(MoveKind::VisualDown),
            Action::CursorLeft => move_to(MoveKind::CharacterLeft),
            Action::CursorRight => move_to(MoveKind::CharacterRight),
            Action::WordLeft => move_to(MoveKind::WordLeft),
            Action::WordRight => move_to(MoveKind::WordRight),
            Action::Home => move_to(MoveKind::LineStart),
            Action::End => move_to(MoveKind::LineEnd),
            Action::PageUp => move_to(MoveKind::PageUp),
            Action::PageDown => move_to(MoveKind::PageDown),
            Action::DeleteNext => Self::DeleteForward,
            Action::DeleteWordLeft => Self::DeleteWordLeft,
            Action::DeleteWordRight => Self::DeleteWordRight,
            Action::DeleteToLineStart => Self::DeleteToLineStart,
            Action::DeleteToLineEnd => Self::DeleteToLineEnd,
            Action::InsertNewline => Self::InsertNewline,
            Action::ComposerShortcut(shortcut) => shortcut,
            Action::RemappedByte(byte) => return Self::from_control_byte(byte),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::input_action::MouseWheel;
    use super::*;

    #[test]
    fn composer_shortcut_raw_control_bytes_map_only_typing_edit_actions() {
        let cases = [
            (1, move_to(MoveKind::LineStart)),
            (5, move_to(MoveKind::LineEnd)),
            (2, move_to(MoveKind::CharacterLeft)),
            (6, move_to(MoveKind::CharacterRight)),
            (14, ShortcutAction::HistoryNext),
            (4, ShortcutAction::DeleteForward),
            (11, ShortcutAction::DeleteToLineEnd),
            (21, ShortcutAction::DeleteToLineStart),
            (23, ShortcutAction::DeleteWhitespaceWordLeft),
            (25, ShortcutAction::Yank),
            (31, ShortcutAction::Undo),
            (12, ShortcutAction::Redraw),
            (127, ShortcutAction::DeleteBackward),
            (8, ShortcutAction::DeleteBackward),
            (b'\n', ShortcutAction::InsertNewline),
        ];
        for (byte, action) in cases {
            assert_eq!(ShortcutAction::from_control_byte(byte), Some(action));
        }
    }

    #[test]
    fn composer_shortcut_table_excludes_app_and_terminal_controls() {
        for byte in [3, 15, 16, 18, 22, 24, b'\r', b'\t', 0x1b] {
            assert_eq!(ShortcutAction::from_control_byte(byte), None);
        }
        let excluded = [
            Action::Escape,
            Action::PasteStart,
            Action::PasteEnd,
            Action::ToggleFullTranscript,
            Action::TogglePermissionMode,
            Action::MouseWheel(MouseWheel::Up),
            Action::RemappedByte(3),
            Action::RemappedByte(15),
            Action::RemappedByte(18),
            Action::RemappedByte(24),
        ];
        for action in excluded {
            assert_eq!(ShortcutAction::from_escape_action(action), None);
        }
    }

    #[test]
    fn composer_shortcut_table_maps_composer_parser_actions() {
        let cases = [
            (Action::CursorUp, move_to(MoveKind::VisualUp)),
            (Action::CursorDown, move_to(MoveKind::VisualDown)),
            (Action::CursorLeft, move_to(MoveKind::CharacterLeft)),
            (Action::CursorRight, move_to(MoveKind::CharacterRight)),
            (Action::WordLeft, move_to(MoveKind::WordLeft)),
            (Action::WordRight, move_to(MoveKind::WordRight)),
            (Action::Home, move_to(MoveKind::LineStart)),
            (Action::End, move_to(MoveKind::LineEnd)),
            (Action::PageUp, move_to(MoveKind::PageUp)),
            (Action::PageDown, move_to(MoveKind::PageDown)),
            (Action::DeleteNext, ShortcutAction::DeleteForward),
            (Action::DeleteWordLeft, ShortcutAction::DeleteWordLeft),
            (Action::DeleteWordRight, ShortcutAction::DeleteWordRight),
            (Action::DeleteToLineStart, ShortcutAction::DeleteToLineStart),
            (Action::DeleteToLineEnd, ShortcutAction::DeleteToLineEnd),
            (Action::InsertNewline, ShortcutAction::InsertNewline),
            (Action::RemappedByte(2), move_to(MoveKind::CharacterLeft)),
            (Action::RemappedByte(6), move_to(MoveKind::CharacterRight)),
        ];
        for (input, action) in cases {
            assert_eq!(ShortcutAction::from_escape_action(input), Some(action));
        }
    }
}
