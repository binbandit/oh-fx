use super::input_action::{
    Action, MousePointer, MousePointerKind, MouseWheel, MoveIntent, MoveKind, ShortcutAction,
};

const SGR_MOUSE_MAX_BYTES: u8 = 18;
const CONTROL_SEQUENCE_DISCARD_MAX_BYTES: u16 = 32;
const KITTY_UP_KEY: u16 = 57352;
const KITTY_DOWN_KEY: u16 = 57353;
const KP_0: u16 = 57399;
const KP_9: u16 = 57408;
const KP_DECIMAL: u16 = 57409;
const KP_DIVIDE: u16 = 57410;
const KP_MULTIPLY: u16 = 57411;
const KP_SUBTRACT: u16 = 57412;
const KP_ADD: u16 = 57413;
const KP_ENTER: u16 = 57414;
const KP_EQUAL: u16 = 57415;
const KP_LEFT: u16 = 57417;
const KP_RIGHT: u16 = 57418;
const KP_UP: u16 = 57419;
const KP_DOWN: u16 = 57420;
const KP_PAGE_UP: u16 = 57421;
const KP_PAGE_DOWN: u16 = 57422;
const KP_HOME: u16 = 57423;
const KP_END: u16 = 57424;
const KP_DELETE: u16 = 57426;
const SHIFT: u16 = 0x01;
const ALT: u16 = 0x02;
const CTRL: u16 = 0x04;
const SUPER: u16 = 0x08;
const KEYPAD_SUPPORTED_MODIFIERS: u16 = SHIFT | ALT | CTRL | SUPER;
const LOCK_STATE_MASK: u16 = 0x3f;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Stage {
    #[default]
    Idle,
    Escape,
    CsiEntry,
    CsiFirstParam,
    Ss3,
    CsiSecondParam,
    CsiThirdParam,
    SgrMouseButton,
    SgrMouseColumn,
    SgrMouseRow,
    X10Button,
    X10Column,
    X10Row,
    DiscardedSgrMouse,
    DiscardedX10Mouse,
    ControlSequenceDiscard,
    KittyEventType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseReportDiscardResult {
    Pending,
    ReportEnd,
    Bound,
    Restart,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MouseInput {
    button: u16,
    column: u16,
    row: u16,
    sgr_bytes: u8,
    discard_remaining: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EscapeParser {
    pub(crate) stage: Stage,
    pub(crate) meta: bool,
    pub(crate) param: u16,
    pub(crate) param2: u16,
    mouse: MouseInput,
}

impl EscapeParser {
    pub(crate) fn begin(&mut self) {
        *self = Self {
            stage: Stage::Escape,
            ..Self::default()
        };
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }

    pub(crate) fn is_bare_escape(&self) -> bool {
        self.stage == Stage::Escape
    }

    pub(crate) fn is_plain_bare_escape(&self) -> bool {
        self.stage == Stage::Escape && !self.meta
    }

    pub(crate) fn is_legacy_x10_payload(&self) -> bool {
        matches!(
            self.stage,
            Stage::X10Button | Stage::X10Column | Stage::X10Row
        )
    }

    pub(crate) fn is_mouse_report_payload(&self) -> bool {
        matches!(
            self.stage,
            Stage::SgrMouseButton | Stage::SgrMouseColumn | Stage::SgrMouseRow
        ) || self.is_legacy_x10_payload()
    }

    pub(crate) fn is_mouse_report_discard(&self) -> bool {
        matches!(
            self.stage,
            Stage::DiscardedSgrMouse | Stage::DiscardedX10Mouse
        )
    }

    pub(crate) fn is_control_sequence_discard(&self) -> bool {
        self.stage == Stage::ControlSequenceDiscard
    }

    pub(crate) fn begin_mouse_report_discard(&mut self) -> bool {
        let remaining = match self.stage {
            Stage::SgrMouseButton | Stage::SgrMouseColumn | Stage::SgrMouseRow => {
                SGR_MOUSE_MAX_BYTES.saturating_sub(self.mouse.sgr_bytes)
            }
            Stage::X10Button => 3,
            Stage::X10Column => 2,
            Stage::X10Row => 1,
            _ => return false,
        };
        if remaining == 0 {
            self.reset_decode();
            return true;
        }
        self.mouse.discard_remaining = remaining;
        self.param = 0;
        self.param2 = 0;
        self.stage = if self.is_legacy_x10_payload() {
            Stage::DiscardedX10Mouse
        } else {
            Stage::DiscardedSgrMouse
        };
        true
    }

    pub(crate) fn consume_mouse_report_discard_byte(
        &mut self,
        byte: u8,
    ) -> MouseReportDiscardResult {
        debug_assert!(self.is_mouse_report_discard());
        if byte == 0x1b {
            self.reset_decode();
            self.stage = Stage::Escape;
            return MouseReportDiscardResult::Restart;
        }
        if self.stage == Stage::DiscardedSgrMouse && matches!(byte, b'M' | b'm') {
            self.reset_decode();
            return MouseReportDiscardResult::ReportEnd;
        }
        self.mouse.discard_remaining = self.mouse.discard_remaining.saturating_sub(1);
        if self.mouse.discard_remaining != 0 {
            return MouseReportDiscardResult::Pending;
        }
        self.reset_decode();
        MouseReportDiscardResult::Bound
    }

    pub(crate) fn consume(&mut self, byte: u8) -> Option<Action> {
        if self.stage == Stage::Idle {
            return None;
        }
        if byte == 0x1b && self.stage != Stage::Escape && !self.is_legacy_x10_payload() {
            self.begin();
            return None;
        }
        match self.stage {
            Stage::Idle => None,
            Stage::Escape => self.consume_escape(byte),
            Stage::CsiEntry => self.consume_csi_entry(byte),
            Stage::ControlSequenceDiscard => self.consume_control_sequence_discard(byte),
            Stage::CsiFirstParam => self.consume_csi_first_param(byte),
            Stage::Ss3 => self.consume_ss3(byte),
            Stage::CsiSecondParam => self.consume_csi_second_param(byte),
            Stage::CsiThirdParam => self.consume_csi_third_param(byte),
            Stage::KittyEventType => self.consume_kitty_event_type(byte),
            Stage::SgrMouseButton | Stage::SgrMouseColumn | Stage::SgrMouseRow => {
                self.consume_sgr_mouse(byte)
            }
            Stage::X10Button => {
                self.mouse.button = u16::from(byte);
                self.stage = Stage::X10Column;
                None
            }
            Stage::X10Column => {
                self.mouse.column = u16::from(byte);
                self.stage = Stage::X10Row;
                None
            }
            Stage::X10Row => {
                let button = self.mouse.button;
                self.reset_decode();
                Some(x10_mouse_action(button))
            }
            Stage::DiscardedSgrMouse | Stage::DiscardedX10Mouse => {
                self.stage = Stage::Idle;
                self.meta = false;
                self.param = 0;
                self.param2 = 0;
                None
            }
        }
    }

    fn consume_escape(&mut self, byte: u8) -> Option<Action> {
        match byte {
            b'[' => {
                self.stage = Stage::CsiEntry;
                self.param = 0;
                self.param2 = 0;
                self.mouse = MouseInput::default();
                None
            }
            b'O' => {
                self.stage = Stage::Ss3;
                self.param = 0;
                self.param2 = 0;
                None
            }
            0x1b => {
                self.meta = true;
                None
            }
            _ => {
                self.reset_decode();
                Some(match byte {
                    b'\r' | b'\n' => Action::InsertNewline,
                    b'b' => Action::WordLeft,
                    b'f' => Action::WordRight,
                    b'd' => Action::ComposerShortcut(ShortcutAction::DeleteWordRight),
                    0x7f | 0x08 => Action::DeleteWordLeft,
                    _ => Action::Ignore,
                })
            }
        }
    }

    fn consume_csi_entry(&mut self, byte: u8) -> Option<Action> {
        let action = match byte {
            b'A' | b'B' | b'C' | b'D' if self.meta => modified_arrow_action(byte, 0, true),
            b'A' | b'B' | b'C' | b'D' | b'H' | b'F' => Some(plain_navigation_action(byte)),
            b'Z' => Some(Action::TogglePermissionMode),
            b'<' => {
                self.mouse = MouseInput::default();
                self.stage = Stage::SgrMouseButton;
                return None;
            }
            b'M' => {
                self.mouse = MouseInput::default();
                self.stage = Stage::X10Button;
                return None;
            }
            b'0'..=b'9' => {
                self.param = u16::from(byte - b'0');
                self.param2 = 1;
                self.stage = Stage::CsiFirstParam;
                return None;
            }
            _ => return self.begin_control_sequence_discard(byte),
        };
        self.reset_decode();
        action
    }

    fn consume_csi_first_param(&mut self, byte: u8) -> Option<Action> {
        match byte {
            b'0'..=b'9' => {
                append_digit_saturating(&mut self.param, byte);
                self.param2 = self.param2.saturating_add(1);
                None
            }
            b';' => {
                self.param2 = self.param;
                self.param = 0;
                self.stage = Stage::CsiSecondParam;
                None
            }
            b'u' => {
                let meta = self.meta;
                let keycode = self.param;
                self.reset_decode();
                Some(kitty_unicode_key_action(keycode, 0, meta))
            }
            b'~' => {
                let value = self.param;
                let digit_count = self.param2;
                self.reset_decode();
                Some(tilde_key_action(value, digit_count))
            }
            _ => self.begin_control_sequence_discard(byte),
        }
    }

    fn consume_ss3(&mut self, byte: u8) -> Option<Action> {
        let action = match byte {
            b'A' | b'B' | b'C' | b'D' if self.meta => {
                modified_arrow_action(byte, 0, true).unwrap_or(Action::Ignore)
            }
            b'A' | b'B' | b'C' | b'D' | b'H' | b'F' => plain_navigation_action(byte),
            b'p'..=b'y' => Action::RemappedByte(b'0' + (byte - b'p')),
            b'l' => Action::RemappedByte(b','),
            b'n' => Action::RemappedByte(b'.'),
            b'j' => Action::RemappedByte(b'*'),
            b'k' => Action::RemappedByte(b'+'),
            b'm' => Action::RemappedByte(b'-'),
            b'o' => Action::RemappedByte(b'/'),
            b'M' => Action::RemappedByte(b'\r'),
            b'X' => Action::RemappedByte(b'='),
            _ => return self.begin_control_sequence_discard(byte),
        };
        self.reset_decode();
        Some(action)
    }

    fn consume_csi_second_param(&mut self, byte: u8) -> Option<Action> {
        let meta = self.meta;
        let modifiers = self.param.saturating_sub(1);
        match byte {
            b'0'..=b'9' => {
                append_digit_saturating(&mut self.param, byte);
                None
            }
            b';' => {
                self.param2 = self.param;
                self.param = 0;
                self.stage = Stage::CsiThirdParam;
                None
            }
            b':' => {
                self.param = modifiers << 4;
                self.stage = Stage::KittyEventType;
                None
            }
            b'u' => {
                let keycode = self.param2;
                self.reset_decode();
                Some(kitty_unicode_key_action(keycode, modifiers, meta))
            }
            b'~' => {
                let keycode = self.param2;
                self.reset_decode();
                Some(modified_tilde_key_action(keycode, modifiers))
            }
            b'Z' if modifiers != 0 => {
                self.reset_decode();
                Some(Action::TogglePermissionMode)
            }
            _ => {
                if let Some(action) = modified_arrow_action(byte, modifiers, meta) {
                    self.reset_decode();
                    return Some(action);
                }
                if !matches!(byte, b'A' | b'B' | b'C' | b'D' | b'H' | b'F') {
                    return self.begin_control_sequence_discard(byte);
                }
                self.reset_decode();
                Some(plain_navigation_action(byte))
            }
        }
    }

    fn consume_csi_third_param(&mut self, byte: u8) -> Option<Action> {
        match byte {
            b'0'..=b'9' => {
                append_digit_saturating(&mut self.param, byte);
                None
            }
            b'~' => {
                let meta = self.meta;
                let keycode = self.param;
                let modifiers = self.param2.saturating_sub(1);
                self.reset_decode();
                Some(kitty_unicode_key_action(keycode, modifiers, meta))
            }
            _ => self.begin_control_sequence_discard(byte),
        }
    }

    fn consume_kitty_event_type(&mut self, byte: u8) -> Option<Action> {
        match byte {
            b'0'..=b'9' => {
                let event_digits = (self.param & 0x0f) * 10 + u16::from(byte - b'0');
                self.param = (self.param & 0xfff0) | event_digits.min(0x0f);
                None
            }
            b'u' => {
                let meta = self.meta;
                let keycode = self.param2;
                let modifiers = self.param >> 4;
                let event_type = self.param & 0x0f;
                self.reset_decode();
                Some(if matches!(event_type, 1 | 2) {
                    kitty_unicode_key_action(keycode, modifiers, meta)
                } else {
                    Action::Ignore
                })
            }
            _ => self.begin_control_sequence_discard(byte),
        }
    }

    fn consume_sgr_mouse(&mut self, byte: u8) -> Option<Action> {
        if !self.note_sgr_mouse_byte()
            || (self.stage != Stage::SgrMouseRow && self.mouse.sgr_bytes == SGR_MOUSE_MAX_BYTES)
        {
            self.reset_decode();
            return Some(Action::Ignore);
        }
        if byte.is_ascii_digit() {
            let target = match self.stage {
                Stage::SgrMouseButton => &mut self.mouse.button,
                Stage::SgrMouseColumn => &mut self.mouse.column,
                _ => {
                    if self.mouse.sgr_bytes == SGR_MOUSE_MAX_BYTES {
                        self.reset_decode();
                        return Some(Action::Ignore);
                    }
                    &mut self.mouse.row
                }
            };
            append_digit_saturating(target, byte);
            return None;
        }
        match (self.stage, byte) {
            (Stage::SgrMouseButton, b';') => {
                self.stage = Stage::SgrMouseColumn;
                None
            }
            (Stage::SgrMouseColumn, b';') => {
                self.stage = Stage::SgrMouseRow;
                None
            }
            (Stage::SgrMouseRow, terminator) => {
                let mouse = self.mouse;
                self.reset_decode();
                Some(sgr_mouse_action(
                    mouse.button,
                    mouse.column,
                    mouse.row,
                    terminator,
                ))
            }
            _ => {
                self.reset_decode();
                Some(Action::Ignore)
            }
        }
    }

    fn note_sgr_mouse_byte(&mut self) -> bool {
        if self.mouse.sgr_bytes == SGR_MOUSE_MAX_BYTES {
            return false;
        }
        self.mouse.sgr_bytes += 1;
        true
    }

    fn consume_control_sequence_discard(&mut self, byte: u8) -> Option<Action> {
        if !(0x20..=0x3f).contains(&byte) {
            self.reset_decode();
            return Some(Action::Ignore);
        }
        self.param = self.param.saturating_add(1);
        if self.param >= CONTROL_SEQUENCE_DISCARD_MAX_BYTES {
            self.reset_decode();
            return Some(Action::Ignore);
        }
        None
    }

    fn begin_control_sequence_discard(&mut self, byte: u8) -> Option<Action> {
        if (0x20..=0x3f).contains(&byte) {
            self.stage = Stage::ControlSequenceDiscard;
            self.param = 1;
            self.param2 = 0;
            self.mouse = MouseInput::default();
            return None;
        }
        self.reset_decode();
        Some(Action::Ignore)
    }

    fn reset_decode(&mut self) {
        *self = Self::default();
    }
}

pub(crate) fn control_byte_feature_action(byte: u8) -> Option<Action> {
    match byte {
        15 => Some(Action::ToggleFullTranscript),
        16 => Some(Action::OpenModelCatalog),
        _ => None,
    }
}

fn append_digit_saturating(param: &mut u16, byte: u8) {
    *param = param
        .saturating_mul(10)
        .saturating_add(u16::from(byte - b'0'));
}

fn composer_move(kind: MoveKind, modifiers: u16) -> Action {
    Action::ComposerShortcut(ShortcutAction::Move(MoveIntent {
        kind,
        extend_selection: modifiers & SHIFT != 0,
    }))
}

fn modified_arrow_action(byte: u8, modifiers: u16, meta_prefixed: bool) -> Option<Action> {
    let kind = if modifiers & SUPER != 0 {
        match byte {
            b'A' | b'H' => MoveKind::DraftStart,
            b'B' | b'F' => MoveKind::DraftEnd,
            b'C' => MoveKind::LineEnd,
            b'D' => MoveKind::LineStart,
            _ => return None,
        }
    } else if meta_prefixed || modifiers & ALT != 0 {
        match byte {
            b'A' => MoveKind::ParagraphUp,
            b'B' => MoveKind::ParagraphDown,
            b'C' => MoveKind::WordRight,
            b'D' => MoveKind::WordLeft,
            _ => return None,
        }
    } else if modifiers & CTRL != 0 {
        match byte {
            b'C' => MoveKind::WordRight,
            b'D' => MoveKind::WordLeft,
            b'A' => MoveKind::VisualUp,
            b'B' => MoveKind::VisualDown,
            b'H' => MoveKind::DraftStart,
            b'F' => MoveKind::DraftEnd,
            _ => return None,
        }
    } else if modifiers & SHIFT != 0 {
        match byte {
            b'A' => MoveKind::VisualUp,
            b'B' => MoveKind::VisualDown,
            b'C' => MoveKind::CharacterRight,
            b'D' => MoveKind::CharacterLeft,
            b'H' => MoveKind::LineStart,
            b'F' => MoveKind::LineEnd,
            _ => return None,
        }
    } else {
        return None;
    };
    Some(composer_move(kind, modifiers))
}

fn plain_navigation_action(letter: u8) -> Action {
    match letter {
        b'A' => Action::CursorUp,
        b'B' => Action::CursorDown,
        b'C' => Action::CursorRight,
        b'D' => Action::CursorLeft,
        b'H' => Action::Home,
        _ => Action::End,
    }
}

fn navigation_key_action(letter: u8, modifiers: u16, meta_prefixed: bool) -> Action {
    if !meta_prefixed && modifiers == 0 {
        return plain_navigation_action(letter);
    }
    modified_arrow_action(letter, modifiers, meta_prefixed)
        .unwrap_or_else(|| plain_navigation_action(letter))
}

fn page_key_action(keycode: u16, modifiers: u16) -> Action {
    let up = keycode == KP_PAGE_UP;
    if modifiers != 0 {
        return composer_move(
            if up {
                MoveKind::PageUp
            } else {
                MoveKind::PageDown
            },
            modifiers,
        );
    }
    if up { Action::PageUp } else { Action::PageDown }
}

fn forward_delete_key_action(modifiers: u16) -> Action {
    if modifiers & SUPER != 0 {
        Action::DeleteToLineEnd
    } else if modifiers & (ALT | CTRL) != 0 {
        Action::DeleteWordRight
    } else if modifiers == 0 {
        Action::DeleteNext
    } else {
        Action::Ignore
    }
}

fn ctrl_o_key_action(meta_prefixed: bool, modifiers: u16) -> Action {
    if !meta_prefixed && modifiers == CTRL {
        Action::ToggleFullTranscript
    } else {
        Action::Ignore
    }
}

fn keypad_character_key(keycode: u16) -> Option<u8> {
    if (KP_0..=KP_9).contains(&keycode) {
        return u8::try_from(keycode - KP_0).ok().map(|digit| b'0' + digit);
    }
    Some(match keycode {
        KP_DECIMAL => b'.',
        KP_DIVIDE => b'/',
        KP_MULTIPLY => b'*',
        KP_SUBTRACT => b'-',
        KP_ADD => b'+',
        KP_EQUAL => b'=',
        _ => return None,
    })
}

fn keypad_navigation_key(keycode: u16) -> Option<u8> {
    Some(match keycode {
        KP_UP => b'A',
        KP_DOWN => b'B',
        KP_RIGHT => b'C',
        KP_LEFT => b'D',
        KP_HOME => b'H',
        KP_END => b'F',
        _ => return None,
    })
}

fn keypad_key_action(
    keycode: u16,
    modifiers: u16,
    mods: u16,
    meta_prefixed: bool,
) -> Option<Action> {
    if mods & !KEYPAD_SUPPORTED_MODIFIERS != 0 {
        return None;
    }
    if let Some(byte) = keypad_character_key(keycode) {
        return (!meta_prefixed && mods & !SHIFT == 0).then_some(Action::RemappedByte(byte));
    }
    if let Some(letter) = keypad_navigation_key(keycode) {
        return Some(navigation_key_action(letter, mods, meta_prefixed));
    }
    match keycode {
        KP_PAGE_UP | KP_PAGE_DOWN => Some(page_key_action(keycode, mods)),
        KP_ENTER => Some(kitty_unicode_key_action(13, modifiers, meta_prefixed)),
        KP_DELETE => Some(forward_delete_key_action(mods)),
        _ => None,
    }
}

fn is_letter(keycode: u16, lower: u8) -> bool {
    keycode == u16::from(lower) || keycode == u16::from(lower.to_ascii_uppercase())
}

fn kitty_unicode_key_action(keycode: u16, modifiers: u16, meta_prefixed: bool) -> Action {
    let mods = modifiers & LOCK_STATE_MASK;
    if keycode == 27 && mods == 0 {
        return Action::Escape;
    }
    if let Some(action) = keypad_key_action(keycode, modifiers, mods, meta_prefixed) {
        return action;
    }
    if keycode == KITTY_UP_KEY || keycode == KITTY_DOWN_KEY {
        let letter = if keycode == KITTY_UP_KEY { b'A' } else { b'B' };
        if meta_prefixed || mods != 0 {
            return modified_arrow_action(letter, mods, meta_prefixed).unwrap_or(Action::Ignore);
        }
        return plain_navigation_action(letter);
    }
    if keycode == 13 && mods == CTRL && !meta_prefixed {
        return Action::RemappedByte(b'\r');
    }
    if keycode == 13 && mods & (SHIFT | ALT) != 0 {
        return Action::InsertNewline;
    }
    if keycode == u16::from(b' ') && mods == SHIFT && !meta_prefixed {
        return Action::RemappedByte(b' ');
    }
    if mods & SUPER != 0 {
        if is_letter(keycode, b'a') {
            return Action::ComposerShortcut(ShortcutAction::SelectAll);
        }
        if is_letter(keycode, b'c') || is_letter(keycode, b'x') {
            return Action::Ignore;
        }
        if is_letter(keycode, b'z') {
            return Action::ComposerShortcut(if mods & SHIFT != 0 {
                ShortcutAction::Redo
            } else {
                ShortcutAction::Undo
            });
        }
    }
    if mods & (SHIFT | CTRL) == SHIFT | CTRL {
        let kind = if is_letter(keycode, b'a') {
            Some(MoveKind::LineStart)
        } else if is_letter(keycode, b'b') {
            Some(MoveKind::CharacterLeft)
        } else if is_letter(keycode, b'e') {
            Some(MoveKind::LineEnd)
        } else if is_letter(keycode, b'f') {
            Some(MoveKind::CharacterRight)
        } else {
            None
        };
        if let Some(kind) = kind {
            return composer_move(kind, mods);
        }
    }
    if mods & ALT != 0 {
        if keycode == u16::from(b'b') {
            return if mods & SHIFT != 0 {
                composer_move(MoveKind::WordLeft, mods)
            } else {
                Action::WordLeft
            };
        }
        if keycode == u16::from(b'f') {
            return if mods & SHIFT != 0 {
                composer_move(MoveKind::WordRight, mods)
            } else {
                Action::WordRight
            };
        }
    }
    if is_letter(keycode, b'd') && (meta_prefixed || mods & ALT != 0) {
        return Action::DeleteWordRight;
    }
    if is_letter(keycode, b'o') {
        return ctrl_o_key_action(meta_prefixed, mods);
    }
    if is_letter(keycode, b'r') && mods & SUPER != 0 {
        return Action::OpenAllSessions;
    }
    if keycode == 9 && mods & SHIFT != 0 {
        return Action::TogglePermissionMode;
    }
    if mods & CTRL != 0
        && let Some(byte) = ctrl_remapped_byte(keycode)
    {
        return Action::RemappedByte(byte);
    }
    match keycode {
        127 if mods & SUPER != 0 => Action::DeleteToLineStart,
        127 if mods & ALT != 0 => Action::DeleteWordLeft,
        13 => Action::RemappedByte(b'\r'),
        9 => Action::RemappedByte(b'\t'),
        127 => Action::RemappedByte(127),
        _ => Action::Ignore,
    }
}

fn ctrl_remapped_byte(keycode: u16) -> Option<u8> {
    let byte = u8::try_from(keycode).ok()?;
    match byte {
        b'_' => Some(31),
        b'a'..=b'z' => Some(byte - 96),
        b'A'..=b'Z' => Some(byte - 64),
        _ => None,
    }
}

fn tilde_key_action(value: u16, digit_count: u16) -> Action {
    if digit_count == 3 && value == 200 {
        return Action::PasteStart;
    }
    if digit_count == 3 && value == 201 {
        return Action::PasteEnd;
    }
    match value {
        1 | 7 => Action::Home,
        3 => Action::DeleteNext,
        4 | 8 => Action::End,
        5 => Action::PageUp,
        6 => Action::PageDown,
        _ => Action::Ignore,
    }
}

fn modified_tilde_key_action(keycode: u16, modifiers: u16) -> Action {
    if keycode == 3 {
        return forward_delete_key_action(modifiers);
    }
    if modifiers == 0 {
        return tilde_key_action(keycode, 0);
    }
    let ctrl = modifiers & CTRL != 0;
    let kind = match keycode {
        1 | 7 if ctrl => MoveKind::DraftStart,
        1 | 7 => MoveKind::LineStart,
        4 | 8 if ctrl => MoveKind::DraftEnd,
        4 | 8 => MoveKind::LineEnd,
        5 => MoveKind::PageUp,
        6 => MoveKind::PageDown,
        _ => return Action::Ignore,
    };
    composer_move(kind, modifiers)
}

fn sgr_mouse_action(button: u16, column: u16, row: u16, terminator: u8) -> Action {
    if column == 0 || row == 0 {
        return Action::Ignore;
    }
    if button & 64 != 0 {
        if terminator != b'M' {
            return Action::Ignore;
        }
        return match button & 0b11 {
            0 => Action::MouseWheel(MouseWheel::Up),
            1 => Action::MouseWheel(MouseWheel::Down),
            _ => Action::Ignore,
        };
    }
    if button & 0b11 != 0 {
        return Action::Ignore;
    }
    let kind = match terminator {
        b'm' => MousePointerKind::Release,
        b'M' if button & 32 != 0 => MousePointerKind::Drag,
        b'M' => MousePointerKind::Press,
        _ => return Action::Ignore,
    };
    Action::MousePointer(MousePointer {
        kind,
        column,
        row,
        shift: button & 4 != 0,
        alt: button & 8 != 0,
        ctrl: button & 16 != 0,
    })
}

fn x10_mouse_action(button: u16) -> Action {
    let Some(normalized) = button.checked_sub(32) else {
        return Action::Ignore;
    };
    if normalized & 64 == 0 {
        return Action::Ignore;
    }
    match normalized & 0b11 {
        0 => Action::MouseWheel(MouseWheel::Up),
        1 => Action::MouseWheel(MouseWheel::Down),
        _ => Action::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped() -> EscapeParser {
        let mut parser = EscapeParser::default();
        parser.begin();
        parser
    }

    fn feed(parser: &mut EscapeParser, bytes: &[u8]) -> Option<Action> {
        let mut action = None;
        for byte in bytes {
            action = parser.consume(*byte);
        }
        action
    }

    fn expect_escape_action(bytes: &[u8], expected: Action) {
        let mut parser = escaped();
        assert_eq!(feed(&mut parser, bytes), Some(expected), "{bytes:?}");
        assert_eq!(parser.stage, Stage::Idle, "{bytes:?}");
    }

    fn expect_stepwise(bytes: &[u8], expected: Action) {
        let mut parser = escaped();
        let (last, prefix) = bytes.split_last().unwrap();
        for byte in prefix {
            assert_eq!(parser.consume(*byte), None, "{bytes:?}");
        }
        assert_eq!(parser.consume(*last), Some(expected), "{bytes:?}");
        assert_eq!(parser.stage, Stage::Idle, "{bytes:?}");
    }

    fn move_escape(kind: MoveKind, extend_selection: bool) -> Action {
        Action::ComposerShortcut(ShortcutAction::Move(MoveIntent {
            kind,
            extend_selection,
        }))
    }

    fn remapped(byte: u8) -> Action {
        Action::RemappedByte(byte)
    }

    #[test]
    fn input_escape_parser_separates_plain_up_down_from_history_arrows() {
        expect_escape_action(b"[A", Action::CursorUp);
        expect_escape_action(b"[B", Action::CursorDown);
        expect_escape_action(b"OA", Action::CursorUp);
        expect_escape_action(b"OB", Action::CursorDown);
        expect_escape_action(b"[1;1A", Action::CursorUp);
        expect_escape_action(b"[1;1B", Action::CursorDown);
        expect_escape_action(b"[57352;1u", Action::CursorUp);
        expect_escape_action(b"[57353;1u", Action::CursorDown);
        expect_escape_action(b"[57352u", Action::CursorUp);
        expect_escape_action(b"[57353u", Action::CursorDown);
    }

    #[test]
    fn input_escape_parser_recognizes_unmodified_kitty_escape() {
        expect_escape_action(b"[27u", Action::Escape);
        expect_escape_action(b"[27;1u", Action::Escape);
        expect_escape_action(b"[27;1:1u", Action::Escape);
        expect_escape_action(b"[27;1:2u", Action::Escape);
        expect_escape_action(b"[27;1:3u", Action::Ignore);
        expect_escape_action(b"[27;129u", Action::Escape);
        expect_escape_action(b"[27;129:1u", Action::Escape);
        expect_escape_action(b"[27;65u", Action::Escape);
        expect_escape_action(b"[27;65:1u", Action::Escape);
    }

    #[test]
    fn input_escape_parser_resolves_unmodified_kitty_backspace_to_a_delete_byte() {
        expect_escape_action(b"[127u", remapped(127));
        expect_escape_action(b"[127;1u", remapped(127));
        expect_escape_action(b"[127;3u", Action::DeleteWordLeft);
        expect_escape_action(b"[127;9u", Action::DeleteToLineStart);
        expect_escape_action(b"[13u", remapped(b'\r'));
        expect_escape_action(b"[13;1u", remapped(b'\r'));
        expect_escape_action(b"[9u", remapped(b'\t'));
        expect_escape_action(b"[9;1u", remapped(b'\t'));
    }

    #[test]
    fn input_escape_parser_resolves_unmapped_single_parameter_csi_u_to_ignore() {
        expect_escape_action(b"[49u", Action::Ignore);
        expect_escape_action(b"[111u", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_preserves_modified_arrow_intent() {
        expect_escape_action(b"[57352;2u", move_escape(MoveKind::VisualUp, true));
        expect_escape_action(b"[57352;3u", move_escape(MoveKind::ParagraphUp, false));
        expect_escape_action(b"[57353;5u", move_escape(MoveKind::VisualDown, false));
        expect_escape_action(b"[57353;9u", move_escape(MoveKind::DraftEnd, false));
        expect_escape_action(b"[1;4D", move_escape(MoveKind::WordLeft, true));
    }

    #[test]
    fn input_escape_parser_maps_kitty_keypad_keys_to_their_main_row_characters() {
        expect_escape_action(b"[57399u", remapped(b'0'));
        expect_escape_action(b"[57408u", remapped(b'9'));
        expect_escape_action(b"[57400;1u", remapped(b'1'));
        expect_escape_action(b"[57400;129u", remapped(b'1'));
        expect_escape_action(b"[57409u", remapped(b'.'));
        expect_escape_action(b"[57410u", remapped(b'/'));
        expect_escape_action(b"[57411u", remapped(b'*'));
        expect_escape_action(b"[57412u", remapped(b'-'));
        expect_escape_action(b"[57413u", remapped(b'+'));
        expect_escape_action(b"[57415u", remapped(b'='));
        expect_escape_action(b"[57412;2u", remapped(b'-'));
        expect_escape_action(b"[57410;3u", Action::Ignore);
        expect_escape_action(b"[57410;5u", Action::Ignore);
        expect_escape_action(b"[57412;9u", Action::Ignore);
        expect_escape_action(b"[57412;17u", Action::Ignore);
        expect_escape_action(b"[57412;33u", Action::Ignore);
        expect_escape_action(b"\x1b[57412u", Action::Ignore);
        expect_escape_action(b"\x1b[57412;2u", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_keeps_keypad_enter_navigation_and_delete_contracts() {
        expect_escape_action(b"[57414u", remapped(b'\r'));
        expect_escape_action(b"[57414;1u", remapped(b'\r'));
        expect_escape_action(b"[57414;5u", remapped(b'\r'));
        expect_escape_action(b"[57414;2u", Action::InsertNewline);
        expect_escape_action(b"[57414;3u", Action::InsertNewline);
        expect_escape_action(b"[57419;1u", Action::CursorUp);
        expect_escape_action(b"[57420;1u", Action::CursorDown);
        expect_escape_action(b"[57417;1u", Action::CursorLeft);
        expect_escape_action(b"[57418;1u", Action::CursorRight);
        expect_escape_action(b"[57423;1u", Action::Home);
        expect_escape_action(b"[57424;1u", Action::End);
        expect_escape_action(b"[57419;2u", move_escape(MoveKind::VisualUp, true));
        expect_escape_action(b"[57417;5u", move_escape(MoveKind::WordLeft, false));
        expect_escape_action(b"[57421u", Action::PageUp);
        expect_escape_action(b"[57422u", Action::PageDown);
        expect_escape_action(b"[57426u", Action::DeleteNext);
        expect_escape_action(b"[3;1~", Action::DeleteNext);
        expect_escape_action(b"[57426;2u", Action::Ignore);
        expect_escape_action(b"[3;2~", Action::Ignore);
        expect_escape_action(b"[57426;3u", Action::DeleteWordRight);
        expect_escape_action(b"[3;3~", Action::DeleteWordRight);
        expect_escape_action(b"[57426;5u", Action::DeleteWordRight);
        expect_escape_action(b"[3;5~", Action::DeleteWordRight);
        expect_escape_action(b"[57426;9u", Action::DeleteToLineEnd);
        expect_escape_action(b"[3;9~", Action::DeleteToLineEnd);
        expect_escape_action(b"[57416u", Action::Ignore);
        expect_escape_action(b"[57425u", Action::Ignore);
        expect_escape_action(b"[57427u", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_resolves_keypad_keys_reported_with_an_event_type() {
        expect_escape_action(b"[57412;1:1u", remapped(b'-'));
        expect_escape_action(b"[57412;1:2u", remapped(b'-'));
        expect_escape_action(b"[57412;1:3u", Action::Ignore);
        expect_escape_action(b"[57412;1:12u", Action::Ignore);
        expect_escape_action(b"[57412;1:0u", Action::Ignore);
        expect_escape_action(b"[57414;1:0u", Action::Ignore);
        expect_escape_action(b"[57410;1:1u", remapped(b'/'));
        expect_escape_action(b"[57414;1:1u", remapped(b'\r'));
        expect_escape_action(b"[57419;1:1u", Action::CursorUp);
    }

    #[test]
    fn input_escape_parser_decodes_application_keypad_ss3_keys() {
        expect_escape_action(b"Op", remapped(b'0'));
        expect_escape_action(b"Oq", remapped(b'1'));
        expect_escape_action(b"Ox", remapped(b'8'));
        expect_escape_action(b"Oy", remapped(b'9'));
        expect_escape_action(b"Ol", remapped(b','));
        expect_escape_action(b"On", remapped(b'.'));
        expect_escape_action(b"Oj", remapped(b'*'));
        expect_escape_action(b"Ok", remapped(b'+'));
        expect_escape_action(b"Om", remapped(b'-'));
        expect_escape_action(b"Oo", remapped(b'/'));
        expect_escape_action(b"OM", remapped(b'\r'));
        expect_escape_action(b"OX", remapped(b'='));
        expect_escape_action(b"OA", Action::CursorUp);
        expect_escape_action(b"OB", Action::CursorDown);
        expect_escape_action(b"OC", Action::CursorRight);
        expect_escape_action(b"OD", Action::CursorLeft);
        expect_escape_action(b"OH", Action::Home);
        expect_escape_action(b"OF", Action::End);
        expect_escape_action(b"OP", Action::Ignore);
        expect_escape_action(b"OQ", Action::Ignore);
        expect_escape_action(b"OR", Action::Ignore);
        expect_escape_action(b"OS", Action::Ignore);
        expect_escape_action(b"OE", Action::Ignore);
        expect_escape_action(b"OI", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_preserves_double_escape_meta_behavior() {
        expect_escape_action(b"\x1b[A", move_escape(MoveKind::ParagraphUp, false));
        expect_escape_action(b"\x1b[B", move_escape(MoveKind::ParagraphDown, false));
        expect_escape_action(b"\x1bOA", move_escape(MoveKind::ParagraphUp, false));
        expect_escape_action(b"\x1bOB", move_escape(MoveKind::ParagraphDown, false));
        expect_escape_action(b"\x1b[D", move_escape(MoveKind::WordLeft, false));
        expect_escape_action(b"\x1b[C", move_escape(MoveKind::WordRight, false));
        expect_escape_action(b"\x1bb", Action::WordLeft);
        expect_escape_action(
            b"\x1bd",
            Action::ComposerShortcut(ShortcutAction::DeleteWordRight),
        );

        let mut parser = escaped();
        assert_eq!(parser.consume(0x1b), None);
        parser = escaped();
        assert_eq!(parser.consume(b'['), None);
        assert_eq!(parser.consume(b'A'), Some(Action::CursorUp));
    }

    #[test]
    fn input_escape_parser_handles_alt_d_as_delete_word_right() {
        expect_escape_action(
            b"d",
            Action::ComposerShortcut(ShortcutAction::DeleteWordRight),
        );
        expect_escape_action(b"[100;3u", Action::DeleteWordRight);
        expect_escape_action(b"[27;3;100~", Action::DeleteWordRight);
    }

    #[test]
    fn input_escape_parser_handles_kitty_alt_b_and_alt_f_as_word_navigation() {
        expect_escape_action(b"[98;3u", Action::WordLeft);
        expect_escape_action(b"[102;3u", Action::WordRight);
    }

    #[test]
    fn input_escape_parser_preserves_shift_on_enhanced_control_and_alt_movement_aliases() {
        expect_escape_action(b"[97;6u", move_escape(MoveKind::LineStart, true));
        expect_escape_action(b"[27;6;97~", move_escape(MoveKind::LineStart, true));
        expect_escape_action(b"[98;6u", move_escape(MoveKind::CharacterLeft, true));
        expect_escape_action(b"[27;6;98~", move_escape(MoveKind::CharacterLeft, true));
        expect_escape_action(b"[101;6u", move_escape(MoveKind::LineEnd, true));
        expect_escape_action(b"[27;6;101~", move_escape(MoveKind::LineEnd, true));
        expect_escape_action(b"[102;6u", move_escape(MoveKind::CharacterRight, true));
        expect_escape_action(b"[27;6;102~", move_escape(MoveKind::CharacterRight, true));
        expect_escape_action(b"[98;4u", move_escape(MoveKind::WordLeft, true));
        expect_escape_action(b"[27;4;98~", move_escape(MoveKind::WordLeft, true));
        expect_escape_action(b"[102;4u", move_escape(MoveKind::WordRight, true));
        expect_escape_action(b"[27;4;102~", move_escape(MoveKind::WordRight, true));
    }

    #[test]
    fn input_escape_parser_handles_plain_csi_cursor_arrows() {
        let mut parser = escaped();
        assert_eq!(parser.consume(b'['), None);
        assert_eq!(parser.stage, Stage::CsiEntry);
        assert_eq!(parser.consume(b'A'), Some(Action::CursorUp));
        expect_stepwise(b"[B", Action::CursorDown);
    }

    #[test]
    fn input_escape_parser_handles_plain_ss3_cursor_arrows() {
        let mut parser = escaped();
        assert_eq!(parser.consume(b'O'), None);
        assert_eq!(parser.stage, Stage::Ss3);
        assert_eq!(parser.consume(b'A'), Some(Action::CursorUp));
        expect_stepwise(b"OB", Action::CursorDown);
    }

    #[test]
    fn input_escape_parser_handles_cursor_movement_and_delete() {
        expect_stepwise(b"[D", Action::CursorLeft);
        expect_stepwise(b"[C", Action::CursorRight);
        expect_stepwise(b"[H", Action::Home);
        expect_stepwise(b"[3~", Action::DeleteNext);
    }

    #[test]
    fn input_escape_parser_handles_page_navigation() {
        expect_stepwise(b"[5~", Action::PageUp);
        expect_stepwise(b"[6~", Action::PageDown);
    }

    #[test]
    fn input_escape_parser_handles_sgr_mouse_wheel_events() {
        expect_stepwise(b"[<64;12;7M", Action::MouseWheel(MouseWheel::Up));
        expect_stepwise(b"[<65;12;7M", Action::MouseWheel(MouseWheel::Down));
    }

    #[test]
    fn input_escape_parser_preserves_sgr_left_pointer_actions() {
        let pointer = |kind, column, row| {
            Action::MousePointer(MousePointer {
                kind,
                column,
                row,
                shift: false,
                alt: false,
                ctrl: false,
            })
        };
        expect_escape_action(b"[<0;12;7M", pointer(MousePointerKind::Press, 12, 7));
        expect_escape_action(b"[<32;15;8M", pointer(MousePointerKind::Drag, 15, 8));
        expect_escape_action(b"[<0;15;8m", pointer(MousePointerKind::Release, 15, 8));
    }

    #[test]
    fn input_escape_parser_handles_legacy_x10_mouse_reports() {
        expect_stepwise(b"[M\x60\x2a\x25", Action::MouseWheel(MouseWheel::Up));
        expect_stepwise(b"[M\x61\x2a\x25", Action::MouseWheel(MouseWheel::Down));
        expect_stepwise(b"[M\x20\x2a\x25", Action::Ignore);
        expect_stepwise(b"[M\x1b\x2a\x25", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_bounds_expired_sgr_and_x10_mouse_report_tails() {
        let mut parser = escaped();
        for byte in b"[<64;79" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert!(parser.begin_mouse_report_discard());
        assert!(parser.is_mouse_report_discard());
        for byte in b";12" {
            assert_eq!(
                parser.consume_mouse_report_discard_byte(*byte),
                MouseReportDiscardResult::Pending
            );
        }
        assert_eq!(
            parser.consume_mouse_report_discard_byte(b'M'),
            MouseReportDiscardResult::ReportEnd
        );
        assert_eq!(parser.stage, Stage::Idle);

        let mut parser = escaped();
        for byte in b"[<" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert!(parser.begin_mouse_report_discard());
        let max_sgr_mouse_report_tail_bytes = 18;
        for index in 0..max_sgr_mouse_report_tail_bytes {
            let expected = if index + 1 == max_sgr_mouse_report_tail_bytes {
                MouseReportDiscardResult::Bound
            } else {
                MouseReportDiscardResult::Pending
            };
            assert_eq!(parser.consume_mouse_report_discard_byte(b'x'), expected);
        }
        assert_eq!(parser.stage, Stage::Idle);

        let mut parser = escaped();
        for byte in b"[M" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert!(parser.begin_mouse_report_discard());
        for byte in b"\x60\x2a" {
            assert_eq!(
                parser.consume_mouse_report_discard_byte(*byte),
                MouseReportDiscardResult::Pending
            );
        }
        assert_eq!(
            parser.consume_mouse_report_discard_byte(0x25),
            MouseReportDiscardResult::Bound
        );
        assert_eq!(parser.stage, Stage::Idle);
    }

    #[test]
    fn input_escape_parser_keeps_private_csi_digits_in_the_escape_sequence() {
        let mut parser = escaped();
        for byte in b"[?12" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert!(parser.is_control_sequence_discard());
        assert_eq!(parser.consume(b'h'), Some(Action::Ignore));
        assert_eq!(parser.stage, Stage::Idle);
    }

    #[test]
    fn input_escape_parser_ignores_complete_unknown_csi_and_ss3_sequences() {
        expect_escape_action(b"[I", Action::Ignore);
        expect_escape_action(b"[1;2R", Action::Ignore);
        expect_escape_action(b"[>0q", Action::Ignore);
        expect_escape_action(b"O1;2P", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_keeps_an_unknown_control_sequence_pending_until_its_final_byte() {
        let mut parser = escaped();
        for byte in b"[>0" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert!(parser.is_control_sequence_discard());
        assert_eq!(parser.consume(b'q'), Some(Action::Ignore));
        assert_eq!(parser.stage, Stage::Idle);
    }

    #[test]
    fn input_escape_parser_restarts_from_an_expired_mouse_report_on_escape() {
        let mut parser = escaped();
        for byte in b"[<64" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert!(parser.begin_mouse_report_discard());
        assert_eq!(
            parser.consume_mouse_report_discard_byte(0x1b),
            MouseReportDiscardResult::Restart
        );
        assert_eq!(parser.stage, Stage::Escape);
        assert_eq!(parser.param, 0);
        assert_eq!(parser.param2, 0);
    }

    #[test]
    fn input_escape_parser_handles_alt_enter_as_insert_newline() {
        expect_stepwise(b"\r", Action::InsertNewline);
    }

    #[test]
    fn input_escape_parser_handles_enhanced_alt_enter_as_insert_newline() {
        expect_escape_action(b"[13;3u", Action::InsertNewline);
        expect_escape_action(b"[27;3;13~", Action::InsertNewline);
    }

    #[test]
    fn input_escape_parser_handles_shift_enter_csi_u_sequence() {
        expect_stepwise(b"[13;2u", Action::InsertNewline);
    }

    #[test]
    fn input_escape_parser_preserves_encoded_shift_space_as_text() {
        expect_escape_action(b"[32;2u", remapped(b' '));
        expect_escape_action(b"[27;2;32~", remapped(b' '));
    }

    #[test]
    fn input_escape_parser_handles_shift_tab_sequences() {
        for sequence in [&b"[Z"[..], b"[1;2Z", b"[9;2u", b"[27;2;9~"] {
            expect_stepwise(sequence, Action::TogglePermissionMode);
        }
    }

    #[test]
    fn input_escape_parser_handles_modify_other_keys_shift_enter_sequence() {
        expect_stepwise(b"[27;2;13~", Action::InsertNewline);
    }

    #[test]
    fn input_escape_parser_handles_delivered_command_editing_shortcuts() {
        let shortcut = Action::ComposerShortcut;
        expect_escape_action(b"[97;9u", shortcut(ShortcutAction::SelectAll));
        expect_escape_action(b"[99;9u", Action::Ignore);
        expect_escape_action(b"[120;9u", Action::Ignore);
        expect_escape_action(b"[122;9u", shortcut(ShortcutAction::Undo));
        expect_escape_action(b"[122;10u", shortcut(ShortcutAction::Redo));
        expect_escape_action(b"[95;5u", remapped(31));
        expect_escape_action(b"[27;9;97~", shortcut(ShortcutAction::SelectAll));
        expect_escape_action(b"[27;9;99~", Action::Ignore);
        expect_escape_action(b"[27;9;120~", Action::Ignore);
        expect_escape_action(b"[27;9;122~", shortcut(ShortcutAction::Undo));
        expect_escape_action(b"[27;10;122~", shortcut(ShortcutAction::Redo));
        expect_escape_action(b"[27;5;95~", remapped(31));
    }

    #[test]
    fn input_escape_parser_decodes_bracketed_paste_start_esc_200() {
        expect_stepwise(b"[200~", Action::PasteStart);
    }

    #[test]
    fn input_escape_parser_decodes_bracketed_paste_end_esc_201() {
        expect_stepwise(b"[201~", Action::PasteEnd);
    }

    #[test]
    fn input_escape_parser_only_accepts_canonical_paste_markers() {
        let cases: [(&[u8], Action); 7] = [
            (b"[200~", Action::PasteStart),
            (b"[201~", Action::PasteEnd),
            (b"[0200~", Action::Ignore),
            (b"[00200~", Action::Ignore),
            (b"[0201~", Action::Ignore),
            (b"[200;1~", Action::Ignore),
            (b"[201;1~", Action::Ignore),
        ];
        for (bytes, expected) in cases {
            let mut parser = escaped();
            assert_eq!(feed(&mut parser, bytes), Some(expected));
            assert_eq!(parser, EscapeParser::default());
        }
    }

    #[test]
    fn input_escape_parser_ignores_long_leading_zero_paste_marker_without_overflowing_digit_count()
    {
        let mut parser = escaped();
        assert_eq!(parser.consume(b'['), None);
        for _ in 0..usize::from(u16::MAX) + 4 {
            assert_eq!(parser.consume(b'0'), None);
        }
        for byte in b"200" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert_eq!(parser.consume(b'~'), Some(Action::Ignore));
        assert_eq!(parser, EscapeParser::default());
    }

    #[test]
    fn input_escape_parser_ignores_long_parameterized_false_paste_start_without_overflowing() {
        let mut parser = escaped();
        for byte in b"[200;999999" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert_eq!(parser.consume(b'~'), Some(Action::Ignore));
        assert_eq!(parser, EscapeParser::default());
    }

    #[test]
    fn input_escape_parser_ignores_long_third_csi_parameter_without_overflowing() {
        let mut parser = escaped();
        for byte in b"[27;2;999999" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert_eq!(parser.consume(b'~'), Some(Action::Ignore));
        assert_eq!(parser, EscapeParser::default());
    }

    #[test]
    fn input_escape_parser_rearms_on_fresh_escape_after_incomplete_sequence() {
        let mut parser = escaped();
        for byte in b"[20\x1b" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert_eq!(parser.stage, Stage::Escape);
        assert_eq!(parser.param, 0);
        assert_eq!(parser.param2, 0);
        for byte in b"[200" {
            assert_eq!(parser.consume(*byte), None);
        }
        assert_eq!(parser.consume(b'~'), Some(Action::PasteStart));
    }

    #[test]
    fn input_escape_parser_handles_ctrl_c_csi_u_sequence() {
        expect_stepwise(b"[99;5u", remapped(3));
    }

    #[test]
    fn input_escape_parser_handles_ctrl_v_csi_u_sequence() {
        expect_stepwise(b"[118;5u", remapped(22));
    }

    #[test]
    fn input_escape_parser_admits_raw_ctrl_o_control_byte() {
        assert_eq!(
            control_byte_feature_action(15),
            Some(Action::ToggleFullTranscript)
        );
        assert_eq!(control_byte_feature_action(3), None);
    }

    #[test]
    fn input_escape_parser_admits_raw_ctrl_p_control_byte() {
        assert_eq!(
            control_byte_feature_action(16),
            Some(Action::OpenModelCatalog)
        );
        assert_eq!(control_byte_feature_action(14), None);
    }

    #[test]
    fn input_escape_parser_handles_ctrl_o_csi_u_sequence() {
        expect_stepwise(b"[111;5u", Action::ToggleFullTranscript);
    }

    #[test]
    fn input_escape_parser_ignores_meta_o() {
        expect_stepwise(b"o", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_rejects_ctrl_meta_o_csi_u_sequence() {
        expect_stepwise(b"[111;7u", Action::Ignore);
    }

    #[test]
    fn input_escape_parser_handles_modify_other_keys_ctrl_o() {
        expect_stepwise(b"[27;5;111~", Action::ToggleFullTranscript);
    }

    #[test]
    fn input_escape_parser_handles_esc_b_as_word_left() {
        expect_stepwise(b"b", Action::WordLeft);
    }

    #[test]
    fn input_escape_parser_handles_esc_f_as_word_right() {
        expect_stepwise(b"f", Action::WordRight);
    }

    #[test]
    fn input_escape_parser_handles_alt_arrow_as_word_jump() {
        expect_stepwise(b"[1;3D", move_escape(MoveKind::WordLeft, false));
        expect_stepwise(b"[1;3C", move_escape(MoveKind::WordRight, false));
    }

    #[test]
    fn input_escape_parser_handles_double_esc_as_meta_prefix_for_arrows() {
        expect_stepwise(b"\x1b[A", move_escape(MoveKind::ParagraphUp, false));
    }

    #[test]
    fn input_escape_parser_handles_double_esc_meta_prefix_for_down_arrow() {
        expect_stepwise(b"\x1b[B", move_escape(MoveKind::ParagraphDown, false));
    }

    #[test]
    fn input_escape_parser_handles_esc_0x7f_as_delete_word_left() {
        expect_stepwise(b"\x7f", Action::DeleteWordLeft);
    }

    #[test]
    fn input_escape_parser_handles_esc_0x08_as_delete_word_left() {
        expect_stepwise(b"\x08", Action::DeleteWordLeft);
    }

    #[test]
    fn input_escape_parser_handles_alt_delete_as_delete_word_right() {
        expect_stepwise(b"[3;3~", Action::DeleteWordRight);
    }

    #[test]
    fn input_escape_parser_still_returns_delete_next_for_plain_csi_3() {
        expect_stepwise(b"[3~", Action::DeleteNext);
    }

    #[test]
    fn input_escape_parser_handles_alt_backspace_kitty_as_delete_word_left() {
        expect_stepwise(b"[127;3u", Action::DeleteWordLeft);
    }

    #[test]
    fn input_escape_parser_handles_cmd_backspace_as_delete_to_line_start() {
        expect_stepwise(b"[127;9u", Action::DeleteToLineStart);
    }

    #[test]
    fn input_escape_parser_handles_cmd_fn_delete_as_delete_to_line_end() {
        expect_stepwise(b"[3;9~", Action::DeleteToLineEnd);
    }

    #[test]
    fn input_escape_parser_remaps_kitty_ctrl_u_and_ctrl_k() {
        for (keycode, expected) in [("117", 21), ("107", 11), ("119", 23)] {
            expect_stepwise(format!("[{keycode};5u").as_bytes(), remapped(expected));
        }
    }

    #[test]
    fn input_escape_parser_preserves_existing_tilde_terminated_function_keys() {
        let cases = [
            ("1", Action::Home),
            ("3", Action::DeleteNext),
            ("4", Action::End),
            ("7", Action::Home),
            ("8", Action::End),
        ];
        for (code, action) in cases {
            expect_stepwise(format!("[{code}~").as_bytes(), action);
        }
    }

    #[test]
    fn input_escape_parser_handles_cmd_arrow_as_home_end() {
        expect_stepwise(b"[1;9D", move_escape(MoveKind::LineStart, false));
        expect_stepwise(b"[1;9C", move_escape(MoveKind::LineEnd, false));
        expect_stepwise(b"[1;9A", move_escape(MoveKind::DraftStart, false));
        expect_stepwise(b"[1;9B", move_escape(MoveKind::DraftEnd, false));
    }

    #[test]
    fn input_escape_parser_treats_ctrl_enter_as_ordinary_submit() {
        expect_stepwise(b"[13;5u", remapped(b'\r'));
    }

    #[test]
    fn input_escape_parser_handles_cmd_r_as_all_session_resume_picker() {
        expect_stepwise(b"[114;9u", Action::OpenAllSessions);
    }

    #[test]
    fn input_escape_parser_handles_ctrl_a_ctrl_e_via_kitty_protocol() {
        expect_stepwise(b"[97;5u", remapped(1));
        expect_stepwise(b"[101;5u", remapped(5));
    }

    #[test]
    fn input_escape_parser_handles_ctrl_arrow_as_word_jump() {
        expect_stepwise(b"[1;5D", move_escape(MoveKind::WordLeft, false));
        expect_stepwise(b"[1;5C", move_escape(MoveKind::WordRight, false));
    }
}
