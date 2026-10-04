use ofx_contract::ApprovalDecision;

use super::Shell;
use super::approval_runtime::ApprovalPrompt;
use super::input_question_runtime::{LIMIT_REJECTED, focused_editor_edit, freeform_edit};
use super::question_prompt::{Draft, FreeformEdit, Insertion};
use crate::footer::approval_draft::Amending;
use crate::input::{
    Action, COMPOSER_INPUT_LIMIT_BYTES, DECISION_INPUT_LIMIT_BYTES, InputEvent, PasteOutcome,
    PasteOwner,
};

const NEXT_PLACEHOLDER: &str = "and tell oh-fx what to do next";
const DIFFERENTLY_PLACEHOLDER: &str = "and tell oh-fx what to do differently";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Amended {
    Yes,
    No,
}

impl Amended {
    fn of(decision: ApprovalDecision) -> Option<Self> {
        match decision {
            ApprovalDecision::Once => Some(Self::Yes),
            ApprovalDecision::Deny => Some(Self::No),
            ApprovalDecision::Always => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Amendment {
    yes: Draft,
    no: Draft,
    active: Option<Amended>,
    limit_rejected: bool,
}

impl Amendment {
    pub(super) fn can_amend(decision: ApprovalDecision) -> bool {
        Amended::of(decision).is_some()
    }

    pub(super) fn begin(&mut self, decision: ApprovalDecision) {
        if let Some(choice) = Amended::of(decision) {
            self.active = Some(choice);
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn close(&mut self) {
        self.active = None;
        self.limit_rejected = false;
    }

    pub(super) fn insert(&mut self, text: &str) -> Insertion {
        let line: String = text
            .chars()
            .map(|character| {
                if matches!(character, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    character
                }
            })
            .filter(|character| !character.is_control())
            .collect();
        let Some(draft) = self.active_draft() else {
            return Insertion::Inactive;
        };
        let inserted = draft.insert(&line, DECISION_INPUT_LIMIT_BYTES);
        if inserted == Insertion::Inserted {
            self.limit_rejected = false;
        }
        inserted
    }

    pub(super) fn note_limit_rejection(&mut self) -> bool {
        !std::mem::replace(&mut self.limit_rejected, true)
    }

    pub(super) fn backspace(&mut self) {
        self.limit_rejected = false;
        if let Some(draft) = self.active_draft() {
            draft.backspace();
        }
    }

    pub(super) fn edit(&mut self, edit: FreeformEdit) {
        self.limit_rejected = false;
        if let Some(draft) = self.active_draft() {
            draft.edit(edit);
        }
    }

    pub(super) fn feedback(&self, decision: ApprovalDecision) -> Option<String> {
        let draft = match Amended::of(decision)? {
            Amended::Yes => &self.yes,
            Amended::No => &self.no,
        };
        (!draft.text.is_empty()).then(|| draft.text.clone())
    }

    pub(super) fn amending(&self) -> Option<Amending<'_>> {
        let (draft, placeholder) = match self.active? {
            Amended::Yes => (&self.yes, NEXT_PLACEHOLDER),
            Amended::No => (&self.no, DIFFERENTLY_PLACEHOLDER),
        };
        Some(Amending {
            draft: &draft.text,
            cursor: draft.cursor,
            placeholder,
        })
    }

    fn active_draft(&mut self) -> Option<&mut Draft> {
        match self.active? {
            Amended::Yes => Some(&mut self.yes),
            Amended::No => Some(&mut self.no),
        }
    }
}

impl Shell<'_> {
    pub(super) fn amend_with(&mut self, event: &InputEvent) -> bool {
        if !self
            .approval
            .as_ref()
            .is_some_and(ApprovalPrompt::amending_open)
        {
            return false;
        }
        match event {
            InputEvent::Raw(raw) => {
                if let Some(edit) = raw.composer_shortcut.and_then(focused_editor_edit) {
                    self.change_draft(|amendment| amendment.edit(edit));
                    return true;
                }
                match raw.byte {
                    b'\t' => {}
                    0x7f | 0x08 => self.change_draft(Amendment::backspace),
                    byte @ 0x20..=0x7e => {
                        self.insert_draft(char::from(byte).encode_utf8(&mut [0; 4]));
                    }
                    _ => return false,
                }
            }
            InputEvent::Action(decoded) => {
                if decoded.action == Action::PasteStart {
                    self.input
                        .begin_paste(PasteOwner::ApprovalDraft, COMPOSER_INPUT_LIMIT_BYTES);
                    return true;
                }
                let Some(edit) = decoded.composer_shortcut.and_then(freeform_edit) else {
                    return false;
                };
                self.change_draft(|amendment| amendment.edit(edit));
            }
            InputEvent::Text(character) => self.insert_draft(character.encode_utf8(&mut [0; 4])),
            InputEvent::Paste(PasteOutcome::Text {
                owner: PasteOwner::ApprovalDraft,
                text,
            }) => self.insert_draft(text),
            InputEvent::Paste(PasteOutcome::LimitExceeded {
                owner: PasteOwner::ApprovalDraft,
                ..
            }) => self.report_draft_limit(),
            InputEvent::Paste(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => return false,
        }
        true
    }

    fn insert_draft(&mut self, text: &str) {
        let mut inserted = Insertion::Inactive;
        self.change_draft(|amendment| inserted = amendment.insert(text));
        if inserted == Insertion::LimitExceeded {
            self.report_draft_limit();
        }
    }

    fn change_draft(&mut self, change: impl FnOnce(&mut Amendment)) {
        let now_ms = self.now_ms();
        if let Some(prompt) = &mut self.approval {
            prompt.note_typed(now_ms);
            change(prompt.amendment_mut());
        }
    }

    fn report_draft_limit(&mut self) {
        if self
            .approval
            .as_mut()
            .is_some_and(|prompt| prompt.amendment_mut().note_limit_rejection())
        {
            self.input_notice(LIMIT_REJECTED);
        }
    }
}
