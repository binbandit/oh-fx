#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseWheel {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MousePointerKind {
    Press,
    Drag,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MousePointer {
    pub(crate) kind: MousePointerKind,
    pub(crate) column: u16,
    pub(crate) row: u16,
    pub(crate) shift: bool,
    pub(crate) alt: bool,
    pub(crate) ctrl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveKind {
    CharacterLeft,
    CharacterRight,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DraftStart,
    DraftEnd,
    ParagraphUp,
    ParagraphDown,
    VisualUp,
    VisualDown,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MoveIntent {
    pub(crate) kind: MoveKind,
    pub(crate) extend_selection: bool,
}

impl MoveIntent {
    pub(crate) fn new(kind: MoveKind) -> Self {
        Self {
            kind,
            extend_selection: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutAction {
    Move(MoveIntent),
    SelectAll,
    CopySelection,
    CutSelection,
    Undo,
    Redo,
    HistoryNext,
    DeleteBackward,
    DeleteForward,
    DeleteWordLeft,
    DeleteWhitespaceWordLeft,
    DeleteWordRight,
    DeleteToLineStart,
    DeleteToLineEnd,
    Yank,
    Redraw,
    InsertNewline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    WordLeft,
    WordRight,
    Home,
    End,
    PageUp,
    PageDown,
    MouseWheel(MouseWheel),
    MousePointer(MousePointer),
    DeleteNext,
    DeleteWordLeft,
    DeleteWordRight,
    DeleteToLineStart,
    DeleteToLineEnd,
    ToggleFullTranscript,
    TogglePermissionMode,
    OpenAllSessions,
    OpenModelCatalog,
    InsertNewline,
    PasteStart,
    PasteEnd,
    ComposerShortcut(ShortcutAction),
    RemappedByte(u8),
    Escape,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawTerminalInput {
    pub(crate) byte: u8,
    pub(crate) composer_shortcut: Option<ShortcutAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedTerminalAction {
    pub(crate) action: Action,
    pub(crate) composer_shortcut: Option<ShortcutAction>,
    pub(crate) cancel_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalInputEvent {
    PasteByte(u8),
    Raw(RawTerminalInput),
    Action(DecodedTerminalAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalDecodeContext {
    pub(crate) now_ms: i64,
    pub(crate) paste_active: bool,
    pub(crate) cancel_pending: bool,
    pub(crate) text_pending: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TerminalInputIngress {
    pub(crate) event: Option<TerminalInputEvent>,
    pub(crate) interrupts_pending_text: bool,
    pub(crate) replay_byte_after_routing: Option<u8>,
}

impl TerminalInputIngress {
    pub(crate) fn set_event(&mut self, event: TerminalInputEvent) {
        debug_assert!(self.event.is_none());
        self.event = Some(event);
    }

    #[cfg(test)]
    pub(crate) fn has_routing_work(self) -> bool {
        self.event.is_some()
            || self.interrupts_pending_text
            || self.replay_byte_after_routing.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_input_ingress_reports_only_effectful_routing_work() {
        assert!(!TerminalInputIngress::default().has_routing_work());
        assert!(
            TerminalInputIngress {
                event: Some(TerminalInputEvent::PasteByte(b'x')),
                ..TerminalInputIngress::default()
            }
            .has_routing_work()
        );
        assert!(
            TerminalInputIngress {
                interrupts_pending_text: true,
                ..TerminalInputIngress::default()
            }
            .has_routing_work()
        );
        assert!(
            TerminalInputIngress {
                replay_byte_after_routing: Some(0x1b),
                ..TerminalInputIngress::default()
            }
            .has_routing_work()
        );
    }
}
