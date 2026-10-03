mod app_input_runtime;
mod app_permission_runtime;
mod app_worker_runtime;
mod approval_runtime;
mod event_loop;
mod input_history_runtime;
mod input_selection_runtime;
mod input_submit_runtime;
pub(crate) mod skills_menu;
mod skills_menu_runtime;
#[cfg(test)]
mod test_shell;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use ofx_contract::{PermissionMode, TurnId, UiCommand};
use ofx_markdown::{Completions, MarkdownProcessor};

pub use app_worker_runtime::{UiEventReceiver, UiEventSender, ui_channel};
pub use input_history_runtime::PromptHistory;

use app_permission_runtime::YoloWarning;
use approval_runtime::ApprovalPrompt;
use input_history_runtime::HistoryRecorder;
use input_selection_runtime::ClipboardRuntime;
use skills_menu::SkillsMenu;

use crate::composer::Composer;
use crate::footer::input_presentation::ComposerView;
use crate::footer::input_presentation::{
    DangerStatus, HintState, compose_hint_row, composer_view, danger_status_text, input_row_limit,
};
use crate::footer::skills_menu_presentation::{skills_menu_hint_row, skills_menu_rows};
use crate::host::Clipboard;
use crate::input::TerminalInput;
use crate::input::gesture_state;
use crate::output::activity_status::{
    ACTIVITY_BLINK_HALF_PERIOD_MS, TurnPhase, TurnTokens, activity_phase, clip_with_ellipsis,
    turn_activity_row,
};
use crate::output::compaction_activity::CompactionStatus;
use crate::render::hint_line;
use crate::render_engine::frame_layout::{LiveParts, solve};
use crate::render_engine::frame_sink::{Frame, FrameSink, LiveRegionRenderer};
use crate::render_engine::transcript_blocks::Entry;
use crate::row_text::Row;
use crate::terminal::signal_pipe::SignalPipe;
use crate::terminal::{
    ColorSupport, ExitCleanup, HistoryReset, Layout, StartupViewport, Terminal, TerminalError,
    interactive_mode_enable_sequence,
};
use crate::theme::Theme;
use crate::transcript::store::Transcript;

const FOOTER_ROWS: u16 = 4;
const STARTUP_MIN_BODY_ROWS: u16 = 11;
const MAX_PROMPT_HISTORY: usize = 100;
const RESIZE_DEBOUNCE_MS: i64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub command: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub category: usize,
    pub compacts: bool,
}

pub struct ShellOptions {
    pub version: String,
    pub model: String,
    pub permission_mode: PermissionMode,
    pub full_access_warning: bool,
    pub workspace_label: String,
    pub workspace_root: PathBuf,
    pub commands: Vec<SlashCommandSpec>,
    pub command_categories: Vec<String>,
    pub prompt_history: PromptHistory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmissionState {
    Queued,
    Active,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Submission {
    prompt: String,
    state: SubmissionState,
    turn_id: Option<TurnId>,
    sequence: u64,
}

struct ActiveTurn {
    turn_id: Option<TurnId>,
    started_ms: i64,
    phase: TurnPhase,
    tokens: TurnTokens,
    markdown: MarkdownProcessor,
    step_break: Option<usize>,
    failure: Option<String>,
}

impl ActiveTurn {
    fn new(prompt: &str, started_ms: i64) -> Self {
        Self {
            turn_id: None,
            started_ms,
            phase: TurnPhase::Thinking,
            tokens: TurnTokens::for_prompt(prompt),
            markdown: MarkdownProcessor::with_completions(Completions::ALL),
            step_break: None,
            failure: None,
        }
    }
}

pub(crate) struct Shell<'a> {
    terminal: Terminal,
    input: TerminalInput,
    composer: Composer,
    history: HistoryRecorder,
    gestures: gesture_state::State,
    transcript: Transcript,
    renderer: LiveRegionRenderer,
    theme: Theme,
    theme_pinned: bool,
    layout: Layout,
    dimensions_invalid: bool,
    options: ShellOptions,
    outstanding: VecDeque<Submission>,
    submitted_prompts: u64,
    turn: Option<ActiveTurn>,
    compaction: Option<CompactionStatus>,
    approval: Option<ApprovalPrompt>,
    skills_menu: Option<SkillsMenu>,
    yolo_warning: YoloWarning,
    events: UiEventReceiver,
    send: Box<dyn FnMut(UiCommand) + 'a>,
    clipboard: ClipboardRuntime,
    signals: SignalPipe,
    clock: Instant,
    resize_due_ms: Option<i64>,
    output: String,
    footer_row: usize,
    should_exit: bool,
    frame: FrameCache,
    metrics: Metrics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Metrics {
    ansi_bytes: usize,
    full_redraws: usize,
    debounced_resizes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreshScreen {
    Erase,
    KeepScrollback,
}

#[derive(Default)]
struct FrameCache {
    stale: bool,
    drawn_activity: Option<i64>,
    composer: Option<ComposerView>,
}

pub fn run_shell(
    options: ShellOptions,
    events: UiEventReceiver,
    clipboard: impl Clipboard + 'static,
    send: impl FnMut(UiCommand),
) -> Result<(), TerminalError> {
    let mut shell = Shell::bootstrap(options, events, Arc::new(clipboard), Box::new(send))?;
    let result = shell.run();
    let fatal = shell.shutdown(result.as_ref().ok().copied().flatten());
    if let Some(signal) = fatal {
        crate::terminal::signal_pipe::raise_default(signal);
    }
    shell.into_clipboard().finish();
    result.map(drop)
}

struct Setup {
    terminal: Terminal,
    signals: SignalPipe,
    input: TerminalInput,
    layout: Layout,
    theme: Theme,
    theme_pinned: bool,
    launch_row: u16,
}

impl<'a> Shell<'a> {
    fn bootstrap(
        options: ShellOptions,
        events: UiEventReceiver,
        clipboard: Arc<dyn Clipboard>,
        send: Box<dyn FnMut(UiCommand) + 'a>,
    ) -> Result<Self, TerminalError> {
        let (signals, mut terminal) = claim_terminal()?;
        let layout = terminal.query_layout(FOOTER_ROWS)?;
        let detection = terminal.detect_theme();
        let theme_pinned = detection.pinned;
        let capabilities = terminal.capabilities();
        let theme = Theme::builtin(
            detection.light,
            theme_pinned,
            capabilities.color == ColorSupport::Truecolor,
        );
        let cursor = terminal.query_cursor_position().ok();
        let typeahead = terminal.take_typeahead();
        let launch_row = StartupViewport::launch_row_from_cursor(cursor, layout);
        let plan = StartupViewport::plan(layout, launch_row, STARTUP_MIN_BODY_ROWS, true);
        terminal.push_launch_rows_into_scrollback(layout, plan.scrollback_rows)?;
        terminal.enter_interactive_mode()?;
        let mut input = TerminalInput::new();
        input.push_bytes(&typeahead);
        if !theme_pinned {
            terminal.enable_theme_notifications()?;
            terminal.request_theme_color_scheme()?;
            input.start_theme_monitor();
        }
        terminal.write_all(title_sequence(&options).as_bytes())?;
        let setup = Setup {
            terminal,
            signals,
            input,
            layout,
            theme,
            theme_pinned,
            launch_row: plan.launch_row,
        };
        Ok(Self::assemble(setup, options, events, clipboard, send))
    }

    fn assemble(
        setup: Setup,
        mut options: ShellOptions,
        events: UiEventReceiver,
        clipboard: Arc<dyn Clipboard>,
        send: Box<dyn FnMut(UiCommand) + 'a>,
    ) -> Self {
        let capabilities = setup.terminal.capabilities();
        let layout = setup.layout;
        let reset_sequence = match capabilities.history_reset {
            HistoryReset::FullReset => format!(
                "\x1bc{}",
                interactive_mode_enable_sequence(capabilities.tmux)
            ),
            HistoryReset::EraseScrollback => String::new(),
        };
        let mut renderer = LiveRegionRenderer::new(
            layout.rows,
            layout.cols,
            capabilities.sync_updates,
            reset_sequence,
        );
        renderer.start_at(usize::from(setup.launch_row.max(1)));
        let mut transcript = Transcript::default();
        transcript.restart(usize::from(layout.cols));
        transcript.push(Entry::Welcome {
            version: options.version.clone(),
        });
        let yolo_warning = YoloWarning::new(options.full_access_warning);
        let mut composer = Composer::new();
        let history = HistoryRecorder::install(options.prompt_history.take(), &mut composer);
        Self {
            terminal: setup.terminal,
            input: setup.input,
            composer,
            history,
            gestures: gesture_state::State::default(),
            transcript,
            renderer,
            theme: setup.theme,
            theme_pinned: setup.theme_pinned,
            layout,
            dimensions_invalid: false,
            options,
            outstanding: VecDeque::new(),
            submitted_prompts: 0,
            turn: None,
            compaction: None,
            approval: None,
            skills_menu: None,
            yolo_warning,
            events,
            send,
            clipboard: ClipboardRuntime::new(clipboard),
            signals: setup.signals,
            clock: Instant::now(),
            resize_due_ms: None,
            output: String::new(),
            footer_row: 0,
            should_exit: false,
            frame: FrameCache {
                stale: true,
                ..FrameCache::default()
            },
            metrics: Metrics::default(),
        }
    }

    fn now_ms(&self) -> i64 {
        i64::try_from(self.clock.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    fn send(&mut self, command: UiCommand) {
        (self.send)(command);
    }

    fn cols(&self) -> usize {
        usize::from(self.layout.cols)
    }

    fn mark_dirty(&mut self) {
        self.frame.stale = true;
    }

    fn invalidate(&mut self) {
        self.mark_dirty();
        self.frame.composer = None;
    }

    fn replay(&mut self) {
        self.metrics.full_redraws += 1;
        self.forget_approval_review();
        self.renderer.resize(self.layout.rows, self.layout.cols);
        self.renderer.reset_screen(&mut self.output);
        self.transcript.restart(self.cols());
        self.invalidate();
    }

    fn forget_approval_review(&mut self) {
        if let Some(prompt) = &mut self.approval {
            prompt.forget_review();
        }
    }

    fn lose_dimensions(&mut self) {
        self.dimensions_invalid = true;
        self.forget_approval_review();
    }

    fn compaction_running(&self) -> bool {
        self.compaction.is_some_and(|status| status.running())
    }

    fn working(&self) -> bool {
        self.turn.is_some() || self.compaction_running()
    }

    fn activity_clock_ms(&self) -> Option<i64> {
        self.compaction
            .and_then(|status| status.clock_ms())
            .or_else(|| self.turn.as_ref().map(|turn| turn.started_ms))
    }

    fn activity_phase(&self, now_ms: i64) -> Option<i64> {
        self.activity_clock_ms()
            .map(|started_ms| activity_phase(started_ms, now_ms))
    }

    fn frame_due(&self, now_ms: i64) -> bool {
        self.frame.stale || self.activity_phase(now_ms) != self.frame.drawn_activity
    }

    fn activity_rows(&self, now_ms: i64) -> Vec<Row> {
        if let Some(status) = &self.compaction {
            return status.rows(&self.theme, now_ms, self.cols());
        }
        self.turn
            .iter()
            .map(|turn| {
                turn_activity_row(
                    &self.theme,
                    turn.phase,
                    turn.started_ms,
                    now_ms,
                    turn.tokens.progress(),
                    self.cols(),
                )
            })
            .collect()
    }

    fn banner_rows(&self) -> Vec<Row> {
        self.outstanding
            .iter()
            .filter(|submission| submission.state == SubmissionState::Queued)
            .map(|submission| {
                let first_line = submission.prompt.lines().next().unwrap_or_default();
                let mut row = Row::styled("┋ ", self.theme.dim);
                row.push(first_line, self.theme.dim);
                clip_with_ellipsis(row, self.cols())
            })
            .collect()
    }

    fn commit_frame(&mut self) -> Result<(), TerminalError> {
        let now_ms = self.now_ms();
        if self.dimensions_invalid || !self.frame_due(now_ms) {
            return self.flush_output();
        }
        self.frame.stale = false;
        self.frame.drawn_activity = self.activity_phase(now_ms);
        let appended = self.transcript.take_new_rows(&self.theme);
        let base_hint = hint_line(
            &self.theme,
            &self.options.model,
            self.options.permission_mode,
            self.cols(),
        );
        let hint_state = HintState {
            ctrl_c_pending: self.gestures.ctrl_c_exit_armed(),
            esc_clear_armed: self.gestures.escape_clear_armed(),
            esc_interrupt_armed: self.gestures.escape_interrupt_armed(),
            danger: if self.yolo_warning.active() && self.approval.is_none() {
                DangerStatus::FullAccess
            } else {
                DangerStatus::None
            },
        };
        let menu = match (&self.skills_menu, &self.approval) {
            (Some(menu), None) => Some(skills_menu_rows(
                menu,
                self.skills_menu_budget(),
                self.cols(),
                &self.theme,
            )),
            _ => None,
        };
        let warning_included =
            menu.is_none() && !danger_status_text(hint_state, self.cols()).is_empty();
        let hint = if menu.is_some() {
            skills_menu_hint_row(&self.theme, self.cols(), hint_state.ctrl_c_pending)
        } else {
            compose_hint_row(&self.theme, &base_hint, hint_state, self.cols())
        };
        let activity = self.activity_rows(now_ms);
        let banner = self.banner_rows();
        let banner_rows = if banner.is_empty() {
            0
        } else {
            banner.len() + 1
        };
        let tail_gap = self.transcript.tail_wants_footer_gap();
        let composer = self
            .frame
            .composer
            .get_or_insert_with(|| match &self.approval {
                Some(prompt) => prompt.view(&self.theme, self.layout, banner_rows),
                None => composer_view(
                    &self.composer,
                    self.layout.cols,
                    input_row_limit(usize::from(self.layout.content_bottom)),
                    &self.theme,
                ),
            });
        let review = composer.review.clone();
        let banner = if review.as_ref().is_some_and(|review| review.screen) {
            Vec::new()
        } else {
            banner
        };
        let live = solve(
            LiveParts {
                tail_gap,
                activity,
                banner,
                composer,
                menu: menu.unwrap_or_default(),
                hint,
            },
            usize::from(self.layout.rows),
        );
        self.footer_row = live.footer_row;
        self.renderer.present(
            &Frame {
                appended: &appended,
                live: &live.rows,
                cursor: live.cursor,
            },
            &mut self.output,
        );
        let hidden_rows = live
            .rows
            .len()
            .saturating_sub(usize::from(self.layout.rows));
        self.flush_output()?;
        let drawn_ms = self.now_ms();
        self.note_frame_committed(drawn_ms, warning_included);
        if let (Some(prompt), Some(review)) = (&mut self.approval, &review) {
            let visible = live.composer_start + review.required_rows.start >= hidden_rows;
            prompt.frame_drawn(self.layout, review, visible, drawn_ms);
        }
        Ok(())
    }

    fn flush_output(&mut self) -> Result<(), TerminalError> {
        if self.output.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.output);
        self.terminal.write_all(bytes.as_bytes())?;
        self.metrics.ansi_bytes += bytes.len();
        Ok(())
    }

    fn exit_cleanup(&self) -> ExitCleanup {
        ExitCleanup {
            footer_top: Some(self.renderer.live_row(self.footer_row)),
            cursor_row: self.renderer.live_row(0),
            rows: self.layout.rows,
            sync_updates: self.terminal.capabilities().sync_updates,
        }
    }

    fn into_clipboard(self) -> ClipboardRuntime {
        self.clipboard
    }

    fn shutdown(&mut self, fatal: Option<i32>) -> Option<i32> {
        let fatal = fatal.or_else(|| self.leave_normally());
        self.signals.uninstall();
        fatal
    }

    fn leave_normally(&mut self) -> Option<i32> {
        let _ = self.flush_output();
        let _ = self.terminal.write_all(b"\x1b]2;\x07");
        let cleanup = self.exit_cleanup();
        let _ = self.terminal.leave_interactive_mode();
        if let Some(signal) = self.pending_fatal_signal() {
            return Some(signal);
        }
        self.terminal.restore_cooked_mode(&cleanup);
        self.pending_fatal_signal()
    }

    fn handle_resize_signal(&mut self, now_ms: i64) {
        self.resize_due_ms = Some(now_ms + RESIZE_DEBOUNCE_MS);
        if self.approval.is_some() {
            self.forget_approval_review();
            self.invalidate();
        }
    }

    fn apply_pending_resize(&mut self, now_ms: i64) {
        let Some(due) = self.resize_due_ms else {
            return;
        };
        if now_ms < due {
            return;
        }
        self.resize_due_ms = None;
        let Ok(layout) = self.terminal.query_layout(FOOTER_ROWS) else {
            self.lose_dimensions();
            return;
        };
        self.metrics.debounced_resizes += 1;
        if self.dimensions_invalid
            || layout.rows != self.layout.rows
            || layout.cols != self.layout.cols
        {
            self.dimensions_invalid = false;
            self.layout = layout;
            self.replay();
        }
    }

    fn repaint_after_stop(&mut self, layout: Option<Layout>) -> Result<(), TerminalError> {
        if !self.theme_pinned {
            self.terminal.enable_theme_notifications()?;
        }
        if let Some(layout) = layout {
            self.layout = layout;
        } else {
            self.lose_dimensions();
            self.handle_resize_signal(self.now_ms());
        }
        self.replay();
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), TerminalError> {
        let cleanup = self.exit_cleanup();
        let _ = self.terminal.write_all(b"\x1b]2;\x07");
        let layout = self
            .terminal
            .suspend_to_job_control(&cleanup, FOOTER_ROWS)?;
        self.terminal
            .write_all(title_sequence(&self.options).as_bytes())?;
        self.repaint_after_stop(layout)
    }

    fn reclaim_after_external_stop(&mut self) -> Result<(), TerminalError> {
        if !self.terminal.reclaim_after_external_stop()? {
            return Ok(());
        }
        let layout = self.terminal.query_layout(FOOTER_ROWS).ok();
        self.repaint_after_stop(layout)
    }

    fn apply_theme(&mut self, light: bool) {
        let truecolor = self.terminal.capabilities().color == ColorSupport::Truecolor;
        let theme = Theme::builtin(light, self.theme_pinned, truecolor);
        if theme != self.theme {
            self.theme = theme;
            self.replay();
        }
    }

    fn push_entry(&mut self, entry: Entry) {
        self.transcript.push(entry);
    }

    fn start_fresh_transcript(&mut self, screen: FreshScreen) {
        match screen {
            FreshScreen::Erase => self.renderer.reset_screen(&mut self.output),
            FreshScreen::KeepScrollback => self.renderer.release_screen(&mut self.output),
        }
        self.transcript.clear();
        self.transcript.restart(self.cols());
        self.push_entry(Entry::Welcome {
            version: self.options.version.clone(),
        });
        self.invalidate();
    }

    fn next_deadline_ms(&self, now_ms: i64) -> Option<i64> {
        let pending_input = self.input.has_pending_input().then_some(now_ms + 10);
        let blink = self.activity_clock_ms().map(|started_ms| {
            started_ms + (activity_phase(started_ms, now_ms) + 1) * ACTIVITY_BLINK_HALF_PERIOD_MS
        });
        [
            pending_input,
            blink,
            self.gestures.next_expiry_ms(),
            self.yolo_warning.deadline_ms(),
            self.resize_due_ms,
            self.compaction.and_then(|status| status.expires_ms()),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

fn claim_terminal() -> Result<(SignalPipe, Terminal), TerminalError> {
    let signals = SignalPipe::install()?;
    let mut terminal = Terminal::open()?;
    terminal.abort_writes_when_readable(signals.fatal_wakeup()?);
    terminal.enable_raw_mode()?;
    Ok((signals, terminal))
}

fn title_sequence(options: &ShellOptions) -> String {
    let label = format!(
        "oh-fx v{} | {}",
        options.version,
        if options.workspace_label.is_empty() {
            "workspace"
        } else {
            &options.workspace_label
        }
    );
    let safe: String = label
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    format!("\x1b]2;{safe}\x07")
}

#[cfg(test)]
mod tests {
    use rustix::process::{Signal, getpid, kill_process};
    use rustix::termios::{self, LocalModes};

    use super::*;
    use crate::terminal::test_pty;

    #[test]
    fn resizing_back_from_a_too_small_terminal_replays_the_screen() {
        let mut test = test_shell::TestShell::start();
        test.screen();
        test.resize(3, 80);
        assert!(test.shell.dimensions_invalid);
        assert!(test.written().is_empty());
        test.resize(24, 80);
        let written = test.written();
        assert!(written.contains("\x1b[2J\x1b[3J"), "{written:?}");
        let screen = test.screen();
        assert!(screen.contains("Run /help for commands"), "{screen}");
        assert!(screen.contains("auto · model-a"), "{screen}");
    }

    #[test]
    fn frames_are_only_built_when_something_visible_changed() {
        let mut test = test_shell::TestShell::start();
        test.screen();
        let now_ms = test.shell.now_ms();
        assert!(!test.shell.frame_due(now_ms));
        assert!(test.written().is_empty());
        test.submit("one");
        assert!(test.shell.frame_due(now_ms));
        assert!(test.screen().contains("┃ one"));
        let started = test.shell.turn.as_ref().unwrap().started_ms;
        assert!(!test.shell.frame_due(started));
        assert!(
            test.shell
                .frame_due(started + ACTIVITY_BLINK_HALF_PERIOD_MS)
        );
        assert!(test.shell.frame.composer.is_some());
        test.type_bytes(b"x");
        test.step();
        assert!(test.shell.frame.composer.is_none());
        assert!(test.screen().contains("┃ x"));
        assert!(test.shell.frame.composer.is_some());
    }

    #[test]
    fn activate_switches_the_current_theme_seen_by_core_producers() {
        let mut test = test_shell::TestShell::start();
        test.screen();
        test.draining(|shell| shell.apply_theme(true));
        assert!(test.shell.theme.light);
        let light = test.written();
        assert!(light.contains("\x1b[2J\x1b[3J"), "{light:?}");
        assert!(light.contains("\x1b[0;1;38;5;235moh-fx"), "{light:?}");
        test.draining(|shell| shell.apply_theme(false));
        assert!(!test.shell.theme.light);
        let dark = test.written();
        assert!(dark.contains("\x1b[0;1;38;5;255moh-fx"), "{dark:?}");
        test.draining(|shell| shell.apply_theme(false));
        assert!(test.written().is_empty());
    }

    #[test]
    fn a_fatal_signal_skips_the_normal_exit_and_keeps_its_restore() {
        let mut test = test_shell::TestShell::start();
        test.screen();
        assert_eq!(test.draining(|shell| shell.shutdown(Some(15))), Some(15));
        let written = test.written();
        assert!(!written.contains("\x1b]2;\x07"), "{written:?}");
        let mut test = test_shell::TestShell::start();
        test.screen();
        assert_eq!(test.draining(|shell| shell.shutdown(None)), None);
        let written = test.written();
        assert!(written.contains("\x1b]2;\x07\x1b[?2031l"), "{written:?}");
    }

    #[test]
    fn a_termination_while_claiming_the_terminal_still_leaves_it_cooked() {
        if test_pty::in_child() {
            let watcher = std::thread::spawn(|| {
                let stdin = rustix::stdio::stdin();
                while termios::tcgetattr(stdin)
                    .unwrap()
                    .local_modes
                    .contains(LocalModes::ICANON)
                {}
                kill_process(getpid(), Signal::TERM).unwrap();
            });
            let (signals, terminal) = claim_terminal().unwrap();
            watcher.join().unwrap();
            let deadline = Instant::now() + test_pty::WAIT;
            while signals.take().fatal.is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            drop(terminal);
            drop(signals);
            std::process::exit(0);
        }
        for _ in 0..5 {
            let pty = test_pty::open();
            let child = test_pty::spawn_on(
                &pty,
                "shell::tests::a_termination_while_claiming_the_terminal_still_leaves_it_cooked",
            );
            let status = test_pty::exit_within(child, test_pty::WAIT);
            let modes = termios::tcgetattr(&pty.slave).unwrap().local_modes;
            assert!(
                status.is_some_and(|status| status.success()) && test_pty::cooked(modes),
                "{status:?} {modes:?}"
            );
        }
    }

    #[test]
    fn the_window_title_names_the_version_and_workspace() {
        let options = ShellOptions {
            version: "0.1.0".to_owned(),
            model: "m".to_owned(),
            permission_mode: PermissionMode::Auto,
            full_access_warning: false,
            workspace_label: "proj\x07".to_owned(),
            workspace_root: PathBuf::from("/proj"),
            commands: Vec::new(),
            command_categories: Vec::new(),
            prompt_history: PromptHistory::disabled(),
        };
        assert_eq!(title_sequence(&options), "\x1b]2;oh-fx v0.1.0 | proj\x07");
    }
}
