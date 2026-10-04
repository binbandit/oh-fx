mod assistant;
mod composer;
mod footer;
mod host;
mod input;
mod list_window;
mod output;
mod render;
mod render_engine;
mod row_text;
mod shell;
mod terminal;
mod theme;
mod transcript;

pub use composer::file_completion_state::{FileMatch, IndexRevision, IndexState, MentionKind};
pub use host::{Clipboard, ForegroundLifecycle, ForegroundState};
pub use shell::{
    DirectoryLister, FileMentionSource, Opening, PromptHistory, ShellOptions, SkillCatalogSource,
    SlashCommandSpec, UiEventReceiver, UiEventSender, run_shell, ui_channel,
};
pub use terminal::TerminalError;
