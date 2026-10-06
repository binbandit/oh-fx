use ofx_contract::{Notice, NoticeTone};
use ofx_markdown::Event;

use super::Shell;
use super::upgrade_shortcut::requests_upgrade;
use crate::input::{Action, InputEvent, MouseWheel, ShortcutAction};
use crate::render_engine::frame_layout::LiveLayout;
use crate::render_engine::frame_sink::{Frame, FrameSink, LiveRegionRenderer};
use crate::render_engine::transcript_blocks::{
    Entry, render_assistant_event, trailing_blank_lines,
};
use crate::row_text::Row;
use crate::terminal::{Layout, TerminalError};
use crate::theme::Theme;

pub(super) struct Screen {
    renderer: LiveRegionRenderer,
    entered: bool,
    layout: Layout,
    render_layout: Layout,
    primary_live: Option<Vec<Row>>,
    primary_changed: bool,
    offset: usize,
    follow_tail: bool,
}

impl Shell<'_> {
    pub(super) fn route_full_transcript_input(
        &mut self,
        event: &InputEvent,
    ) -> Result<bool, TerminalError> {
        if self.full_transcript_input_blocked() {
            self.close_full_transcript()?;
            return Ok(false);
        }
        let toggles = matches!(event, InputEvent::Action(decoded) if decoded.action == Action::ToggleFullTranscript);
        if self
            .full_transcript
            .as_ref()
            .is_some_and(|screen| !screen.entered)
            && !toggles
        {
            self.full_transcript = None;
            return Ok(false);
        }
        if self.full_transcript.is_some() && requests_upgrade(event) {
            self.push_entry(Entry::Notice(Notice::new(
                NoticeTone::Neutral,
                "upgrade",
                "close the transcript view before upgrading",
            )));
            return Ok(true);
        }
        let action = match event {
            InputEvent::Raw(raw) if matches!(raw.byte, 3 | 12) => {
                Some(Action::RemappedByte(raw.byte))
            }
            InputEvent::Action(decoded) => Some(decoded.action),
            _ => None,
        };
        if action == Some(Action::ToggleFullTranscript) {
            if self.full_transcript.is_some() {
                self.close_full_transcript()?;
            } else {
                self.open_full_transcript();
            }
            return Ok(true);
        }
        let Some(screen) = &mut self.full_transcript else {
            return Ok(false);
        };
        let (direction, rows) = match action {
            Some(Action::Escape | Action::RemappedByte(3)) => {
                self.gestures.disarm_ctrl_c_exit();
                self.gestures.disarm_escape_clear();
                self.gestures.disarm_escape_interrupt();
                self.close_full_transcript()?;
                return Ok(true);
            }
            Some(Action::CursorLeft | Action::CursorRight) => return Ok(true),
            Some(Action::CursorUp | Action::MouseWheel(MouseWheel::Up)) => (MouseWheel::Up, 3),
            Some(Action::CursorDown | Action::MouseWheel(MouseWheel::Down)) => {
                (MouseWheel::Down, 3)
            }
            Some(Action::PageUp) => (MouseWheel::Up, usize::from(self.layout.content_bottom)),
            Some(Action::PageDown) => (MouseWheel::Down, usize::from(self.layout.content_bottom)),
            Some(Action::RemappedByte(12) | Action::ComposerShortcut(ShortcutAction::Redraw)) => {
                screen.renderer.reset_screen();
                return Ok(true);
            }
            _ => return Ok(false),
        };
        screen.follow_tail = false;
        screen.offset = match direction {
            MouseWheel::Up => screen.offset.saturating_sub(rows),
            MouseWheel::Down => screen.offset.saturating_add(rows),
        };
        Ok(true)
    }

    fn open_full_transcript(&mut self) {
        self.full_transcript = Some(Screen {
            renderer: LiveRegionRenderer::new(
                self.layout.rows,
                self.layout.cols,
                self.terminal.capabilities().sync_updates,
                "\x1b[0m\x1b[2J\x1b[H".to_owned(),
            ),
            entered: false,
            layout: self.layout,
            render_layout: self.layout,
            primary_live: None,
            primary_changed: false,
            offset: 0,
            follow_tail: true,
        });
    }

    pub(super) fn close_full_transcript(&mut self) -> Result<(), TerminalError> {
        let Some(screen) = &self.full_transcript else {
            return Ok(());
        };
        if !screen.entered {
            self.full_transcript = None;
            self.invalidate();
            return Ok(());
        }
        self.output.push_str("\x1b[?1049l");
        self.flush_output()?;
        if let Some(screen) = self.full_transcript.take()
            && (screen.primary_changed || screen.layout != self.layout)
        {
            self.replay();
        }
        self.invalidate();
        Ok(())
    }

    pub(super) fn reset_full_transcript_frame(&mut self) {
        if let Some(screen) = &mut self.full_transcript {
            screen.renderer.reset_screen();
        }
    }

    pub(super) fn settle_full_transcript_owner(&mut self) -> Result<(), TerminalError> {
        if self.full_transcript_input_blocked() {
            self.close_full_transcript()?;
        }
        Ok(())
    }

    fn full_transcript_input_blocked(&self) -> bool {
        self.approval.is_some()
            || self.question.is_some()
            || self.statusline_menu.is_some()
            || self.settings_menu.is_some()
            || self.skills_menu_visible()
            || self.help_menu.is_some()
            || self.model_menu.is_some()
            || self.model_draft.is_some()
            || self.picker_active()
            || self.has_file_query()
    }

    pub(super) fn present_full_transcript_frame(
        &mut self,
        appended: &[Row],
        live: &LiveLayout,
    ) -> bool {
        let Some(screen) = &mut self.full_transcript else {
            return false;
        };
        if !screen.entered {
            self.footer_row = live.footer_row;
            self.renderer.present(
                &Frame {
                    appended,
                    live: &live.rows,
                    cursor: live.cursor,
                },
                &mut self.output,
            );
            screen.layout = self.layout;
            screen.entered = true;
            self.output.push_str("\x1b[?1049h");
        }
        if let Some(primary) = &screen.primary_live {
            screen.primary_changed |= !appended.is_empty() || *primary != live.rows;
        } else {
            screen.primary_live = Some(live.rows.clone());
        }
        if screen.render_layout != self.layout {
            screen.renderer.resize(self.layout.rows, self.layout.cols);
            screen.renderer.reset_screen();
            screen.render_layout = self.layout;
        }
        let footer = &live.rows[live.footer_row.min(live.rows.len())..];
        let visible = usize::from(self.layout.rows).saturating_sub(footer.len() + 1);
        let entries = self.transcript.full_entries();
        let total = projection_rows(entries, usize::from(self.layout.cols), &self.theme).count();
        let maximum = total.saturating_sub(visible);
        screen.offset = if screen.follow_tail {
            maximum
        } else {
            screen.offset.min(maximum)
        };
        screen.follow_tail = screen.offset == maximum;
        let mut rows = projection_rows(entries, usize::from(self.layout.cols), &self.theme)
            .skip(screen.offset)
            .take(visible)
            .collect::<Vec<_>>();
        rows.resize_with(visible + 1, Row::new);
        let cursor = live
            .cursor
            .map(|(row, column)| (visible + 1 + row.saturating_sub(live.footer_row), column));
        rows.extend_from_slice(footer);
        screen.renderer.present(
            &Frame {
                appended: &[],
                live: &rows,
                cursor,
            },
            &mut self.output,
        );
        true
    }
}

fn projection_rows<'a>(
    entries: &'a [Entry],
    cols: usize,
    theme: &'a Theme,
) -> impl Iterator<Item = Row> + 'a {
    entries.iter().enumerate().flat_map(move |(index, entry)| {
        let gap = (index > 0 && !entries[index - 1].keeps_trailing_blank())
            .then(Row::new)
            .into_iter();
        let (static_rows, events): (Vec<Row>, &[Event]) = match entry {
            Entry::Assistant { events } => (
                Vec::new(),
                &events[..events.len() - trailing_blank_lines(events)],
            ),
            _ => (entry.render(cols, theme), &[]),
        };
        gap.chain(static_rows).chain(
            events
                .iter()
                .flat_map(move |event| render_assistant_event(event, cols, theme)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_shell::TestShell;
    use crate::render_engine::frame_sink::FrameSink;
    use crate::render_engine::transcript_blocks::Entry;
    use crate::terminal::TAGGED_CURSOR_QUERY;

    #[test]
    fn typing_with_native_clear_probing_keeps_the_retained_transcript() {
        let mut test = TestShell::start();
        test.shell.input.start_native_clear_probe();
        test.shell.transcript.push(Entry::UserTurn {
            text: "retained before probing".to_owned(),
        });
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        test.screen();
        let row = test
            .shell
            .full_transcript
            .as_ref()
            .unwrap()
            .renderer
            .cursor_row()
            .unwrap();
        assert_ne!(Some(row), test.shell.renderer.cursor_row());
        test.type_bytes(b"x");
        test.step();
        let written = test.written();
        if written.contains(TAGGED_CURSOR_QUERY) {
            test.type_bytes(format!("\x1b[{row};1R\x1b[{row};2R").as_bytes());
            test.step();
        }
        assert!(test.screen().contains("retained before probing"));
        assert!(!written.contains(TAGGED_CURSOR_QUERY));
        assert_eq!(test.shell.composer.text(), "x");
        test.type_bytes(b"\x0f");
        test.step();
        test.screen();
        let row = test.shell.renderer.cursor_row().unwrap();
        test.type_bytes(b"y");
        test.step();
        assert_eq!(test.written(), TAGGED_CURSOR_QUERY);
        test.type_bytes(format!("\x1b[{row};1R\x1b[{row};2R").as_bytes());
        test.step();
        assert!(test.screen().contains("retained before probing"));
        assert_eq!(test.shell.composer.text(), "xy");
        assert!(test.sent().is_empty());
    }

    #[test]
    fn questions_keep_navigation_before_and_after_viewer_entry() {
        use ofx_contract::{
            QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId, TurnId, UiCommand,
            UiEvent,
        };

        for viewer_first in [false, true] {
            let mut test = TestShell::start();
            test.submit("pick an option");
            test.deliver(UiEvent::TurnStarted {
                turn_id: TurnId::new(1),
            });
            test.type_bytes(b"unfinished draft");
            test.step();
            test.screen();
            if viewer_first {
                test.type_bytes(b"\x0f");
                test.step();
                test.screen();
            }
            test.deliver(UiEvent::QuestionRequested {
                turn_id: TurnId::new(1),
                request: QuestionRequest {
                    id: RequestId::new(7),
                    entries: vec![QuestionBatchEntry {
                        question: "Which option?".to_owned(),
                        options: ["First", "Second"]
                            .into_iter()
                            .map(|label| QuestionOption {
                                label: label.to_owned(),
                                description: None,
                            })
                            .collect(),
                    }],
                },
            });
            if !viewer_first {
                test.type_bytes(b"\x0f");
                test.step();
            }
            test.written();
            test.type_bytes(b"\x1b[B\r");
            test.step();
            assert_eq!(
                test.sent()
                    .into_iter()
                    .filter(|command| matches!(command, UiCommand::QuestionAnswered { .. }))
                    .collect::<Vec<_>>(),
                [UiCommand::QuestionAnswered {
                    request_id: RequestId::new(7),
                    answers: Some(vec!["Second".to_owned()]),
                }]
            );
            assert!(test.shell.full_transcript.is_none());
            assert_eq!(test.shell.composer.text(), "unfinished draft");
            assert!(
                !test
                    .sent()
                    .iter()
                    .any(|command| matches!(command, UiCommand::Cancel { .. }))
            );
        }
    }

    #[test]
    fn the_help_menu_keeps_navigation_before_and_after_viewer_entry() {
        use ofx_contract::UiEvent;

        for viewer_first in [false, true] {
            let mut test = TestShell::start();
            test.screen();
            if viewer_first {
                test.type_bytes(b"\x0f");
                test.step();
                test.screen();
            }
            test.type_bytes(b"/");
            test.step();
            test.deliver(UiEvent::HelpRequested);
            test.screen();
            if !viewer_first {
                test.type_bytes(b"\x0f");
                test.step();
            }
            assert_eq!(test.shell.help_menu.unwrap().selected(), 0);
            test.type_bytes(b"\x1b[B");
            test.step();
            assert_eq!(test.shell.help_menu.unwrap().selected(), 1);
            assert!(test.shell.full_transcript.is_none());
            assert_eq!(test.shell.composer.text(), "/");
            assert!(test.sent().is_empty());
        }
    }

    #[test]
    fn ctrl_o_opens_the_transcript_and_restores_the_inline_draft() {
        let mut test = TestShell::start();
        test.shell.transcript.push(Entry::UserTurn {
            text: "A retained prompt".to_owned(),
        });
        test.type_bytes(b"unfinished draft");
        test.step();
        let before = test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        assert!(test.written().contains("\x1b[?1049h"));
        assert!(test.screen().contains("A retained prompt"));
        assert!(test.screen().contains("unfinished draft"));
        test.type_bytes(b"\x0f");
        test.step();
        assert!(test.written().contains("\x1b[?1049l"));
        assert_eq!(test.screen(), before);
        assert!(test.sent().is_empty());
    }

    #[test]
    fn ctrl_c_closes_the_viewer_without_submitting_or_cancelling() {
        let mut test = TestShell::start();
        test.type_bytes(b"draft");
        test.step();
        let before = test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        test.written();
        test.type_bytes(b"\x03");
        test.step();
        assert!(test.written().contains("\x1b[?1049l"));
        assert_eq!(test.screen(), before);
        assert!(test.sent().is_empty());
    }

    #[test]
    fn ordinary_viewer_entry_and_exit_preserve_native_mouse_selection() {
        for close in [b"\x0f".as_slice(), b"\x03", b"\x1b"] {
            let mut test = TestShell::start();
            test.screen();
            test.type_bytes(b"\x0f");
            test.step();
            let entered = test.written();
            assert_eq!(entered.matches("\x1b[?1049h").count(), 1);
            test.type_bytes(close);
            test.step();
            test.advance(40);
            test.settle();
            let left = test.written();
            assert_eq!(left.matches("\x1b[?1049l").count(), 1);
            for sequence in ["\x1b[?1000h", "\x1b[?1006h", "\x1b[?1000l", "\x1b[?1006l"] {
                assert!(!entered.contains(sequence), "{entered:?}");
                assert!(!left.contains(sequence), "{left:?}");
            }
            assert!(test.sent().is_empty());
        }
    }

    #[test]
    fn reported_wheel_input_scrolls_three_rows_without_editing_the_draft() {
        let mut test = TestShell::start();
        for index in 0..40 {
            test.shell.transcript.push(Entry::UserTurn {
                text: format!("retained-{index:02}"),
            });
        }
        test.type_bytes(b"draft");
        test.step();
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        test.screen();
        let tail = test.shell.full_transcript.as_ref().unwrap().offset;
        assert!(tail > 3);
        test.type_bytes(b"\x1b[<64;1;1M");
        test.step();
        test.screen();
        assert_eq!(
            test.shell.full_transcript.as_ref().unwrap().offset,
            tail - 3
        );
        test.type_bytes(b"\x1b[<65;1;1M");
        test.step();
        test.screen();
        assert_eq!(test.shell.full_transcript.as_ref().unwrap().offset, tail);
        assert_eq!(test.shell.composer.text(), "draft");
        assert!(test.sent().is_empty());
    }

    #[test]
    fn viewer_navigation_leaves_the_editable_draft_cursor_alone() {
        let mut test = TestShell::start();
        test.type_bytes(b"\x1b[111;5u");
        test.step();
        assert!(test.written().contains("\x1b[?1049h"));
        test.type_bytes(b"ab\x1b[D\x1b[C\x18");
        test.step();
        assert_eq!(test.shell.composer.text(), "ab");
        assert_eq!(test.shell.composer.cursor(), 2);
        test.type_bytes(b"\x1b[111;5u");
        test.step();
        assert!(test.written().contains("\x1b[?1049l"));
        assert!(test.screen().contains("ab"));
        assert!(test.sent().is_empty());
    }

    #[test]
    fn a_pending_open_waits_for_primary_damage_and_escape_cancels_it() {
        let mut test = TestShell::start();
        test.screen();
        test.shell.dimensions_invalid = true;
        test.type_bytes(b"\x0f");
        test.step();
        assert!(!test.written().contains("\x1b[?1049h"));
        test.type_bytes(b"\x1b");
        test.step();
        test.advance(40);
        test.settle();
        assert!(!test.written().contains("\x1b[?1049l"));
        test.shell.dimensions_invalid = false;
        test.shell.invalidate();
        assert!(!test.written().contains("\x1b[?1049h"));
    }

    #[test]
    fn closing_the_viewer_leaves_a_running_turn_and_its_live_reply_intact() {
        use ofx_contract::{TurnId, UiEvent};

        for close in [b"\x03".as_slice(), b"\x0f".as_slice()] {
            let mut test = TestShell::start();
            test.submit("go");
            test.deliver(UiEvent::TurnStarted {
                turn_id: TurnId::new(1),
            });
            test.screen();
            let commands = test.sent();
            test.type_bytes(b"\x0f");
            test.step();
            test.deliver(UiEvent::AssistantText {
                turn_id: TurnId::new(1),
                text: "a live reply\n\n".to_owned(),
            });
            assert!(test.screen().contains("a live reply"));
            test.type_bytes(close);
            test.step();
            assert!(test.written().contains("\x1b[?1049l"));
            assert_eq!(test.sent(), commands);
            assert!(test.screen().contains("a live reply"));
        }
    }

    #[test]
    fn a_terminal_repaint_repairs_the_active_alternate_buffer() {
        let mut test = TestShell::start();
        test.type_bytes(b"draft");
        test.step();
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        let before = test.screen();
        test.draining(|shell| {
            shell
                .terminal
                .write_all(b"\x1b[1;1Hdamaged terminal")
                .unwrap();
        });
        assert!(test.screen().contains("damaged terminal"));
        test.draining(|shell| shell.repaint_after_stop(Some(shell.layout)).unwrap());
        assert_eq!(test.screen(), before);
        assert!(test.sent().is_empty());
    }

    #[test]
    fn an_approval_takes_input_after_the_viewer_restores_inline() {
        use ofx_contract::{
            ApprovalOrigin, ApprovalRequest, ApprovalScope, CallDescription, Concurrency,
            PathAccess, RequestId, ToolActivity, ToolCallId, ToolEffect, TurnId, UiEvent,
        };

        let mut test = TestShell::start();
        test.submit("read notes");
        test.deliver(UiEvent::TurnStarted {
            turn_id: TurnId::new(1),
        });
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        test.screen();
        let commands = test.sent();
        test.deliver(UiEvent::ApprovalRequested {
            turn_id: TurnId::new(1),
            request: Box::new(ApprovalRequest {
                id: RequestId::new(7),
                tool_name: "read_file".to_owned(),
                call_id: ToolCallId::new("read-notes"),
                description: CallDescription {
                    title: "Reading notes.txt".to_owned(),
                    label: None,
                    activity: ToolActivity::Read,
                    effect: ToolEffect::ReadOnly,
                    concurrency: Concurrency::Serial,
                },
                tool_arguments_preview: "{}".to_owned(),
                tool_arguments_truncated: false,
                scope: ApprovalScope {
                    target: None,
                    access: PathAccess::WorkspaceOnly,
                    always: None,
                },
                command: None,
                file: None,
                origin: ApprovalOrigin::ActiveSession,
                change: None,
            }),
        });
        assert!(test.written().contains("\x1b[?1049l"));
        assert!(test.shell.approval.is_some());
        test.type_bytes(b"\x0f");
        test.step();
        assert!(!test.written().contains("\x1b[?1049h"));
        assert!(test.shell.approval.is_some());
        assert_eq!(test.sent(), commands);
    }

    #[test]
    fn closing_after_resize_restores_the_same_inline_shell_as_an_inline_resize() {
        let mut viewer = TestShell::start();
        let mut inline = TestShell::start();
        for test in [&mut viewer, &mut inline] {
            test.shell.transcript.push(Entry::UserTurn {
                text: "retained words that wrap across a narrower terminal width".to_owned(),
            });
            test.type_bytes(b"a draft that also wraps across a narrower terminal width");
            test.step();
            test.screen();
        }
        viewer.type_bytes(b"\x0f");
        viewer.step();
        viewer.screen();
        viewer.resize(24, 30);
        inline.resize(24, 30);
        viewer.screen();
        viewer.type_bytes(b"\x0f");
        viewer.step();
        assert_eq!(viewer.screen(), inline.screen());
        assert!(viewer.sent().is_empty());
    }

    #[test]
    fn complete_navigation_input_cancels_a_pending_open() {
        for key in [b"\x1b[A".as_slice(), b"\x1b[B", b"\x1b[5~", b"\x1b[6~"] {
            let mut test = TestShell::start();
            test.screen();
            test.shell.dimensions_invalid = true;
            test.type_bytes(b"\x0f");
            test.step();
            test.type_bytes(key);
            test.step();
            test.shell.dimensions_invalid = false;
            test.shell.invalidate();
            assert!(!test.written().contains("\x1b[?1049h"));
            assert_eq!(test.shell.composer.text(), "");
            assert!(test.sent().is_empty());
        }
    }

    #[test]
    fn the_skills_catalog_keeps_ctrl_o_without_opening_another_screen() {
        let mut test = TestShell::start();
        test.shell
            .open_skills_menu(Vec::new(), &ofx_contract::SkillMenuFocus::Start);
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        let output = test.written();
        assert!(!output.contains("\x1b[?1049h"), "{output:?}");
        assert!(test.shell.command_skills_menu_open());
        assert!(test.sent().is_empty());
    }

    #[test]
    fn typing_cancels_a_pending_open_without_losing_the_draft() {
        let mut test = TestShell::start();
        test.screen();
        test.shell.dimensions_invalid = true;
        test.type_bytes(b"\x0f");
        test.step();
        test.type_bytes(b"draft");
        test.step();
        test.shell.dimensions_invalid = false;
        test.shell.invalidate();
        let output = test.written();
        assert!(!output.contains("\x1b[?1049h"), "{output:?}");
        assert!(!output.contains("\x1b[?1049l"), "{output:?}");
        assert_eq!(test.shell.composer.text(), "draft");
        assert!(test.sent().is_empty());
    }

    #[test]
    fn pending_primary_rows_are_committed_before_the_alternate_buffer_opens() {
        let mut test = TestShell::start();
        test.screen();
        test.shell.transcript.push(Entry::UserTurn {
            text: "primary damage".to_owned(),
        });
        test.type_bytes(b"\x0f");
        test.step();
        let output = test.written();
        let primary = output.find("primary damage").unwrap();
        let alternate = output.find("\x1b[?1049h").unwrap();
        assert!(primary < alternate, "{output:?}");
        test.type_bytes(b"\x0f");
        test.step();
        assert!(test.screen().contains("primary damage"));
    }

    #[test]
    fn retained_hostile_text_is_safe_in_the_alternate_buffer() {
        let mut test = TestShell::start();
        test.shell.transcript.push(Entry::UserTurn {
            text: "unsafe\u{1b}]0;renamed\u{7}\u{202e}".to_owned(),
        });
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        let output = test.written();
        assert!(!output.contains("\u{1b}]0;"), "{output:?}");
        assert!(!output.contains('\u{7}'), "{output:?}");
        assert!(!output.contains('\u{202e}'), "{output:?}");
        let screen = test.screen();
        assert!(screen.contains("unsafe"), "{screen}");
    }

    #[test]
    fn the_viewer_opens_at_the_tail_and_pages_back_through_retained_prompts() {
        let mut test = TestShell::start();
        for index in 0..40 {
            test.shell.transcript.push(Entry::UserTurn {
                text: format!("retained-{index:02}"),
            });
        }
        test.screen();
        test.type_bytes(b"\x0f");
        test.step();
        let tail = test.screen();
        assert!(tail.contains("retained-39"), "{tail}");
        assert!(!tail.contains("retained-00"), "{tail}");
        for _ in 0..5 {
            test.type_bytes(b"\x1b[5~");
            test.step();
            test.screen();
        }
        let first = test.screen();
        assert!(first.contains("retained-00"), "{first}");
        assert!(!first.contains("retained-39"), "{first}");
    }
}
