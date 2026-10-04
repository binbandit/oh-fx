use ofx_cli::{
    HelpStyle, TOP_LEVEL_HELP_DEFAULT_WIDTH, TopLevelKind, render_command_help,
    render_top_level_help,
};

const VERSION: &str = "9.8.7";

fn top_level(columns: usize, style: HelpStyle) -> String {
    render_top_level_help(columns, VERSION, style)
}

fn command(kind: TopLevelKind) -> String {
    render_command_help(kind)
}

#[test]
fn top_level_help_matches_golden_at_default_width() {
    assert_eq!(
        top_level(TOP_LEVEL_HELP_DEFAULT_WIDTH, HelpStyle::Plain),
        include_str!("golden/help_plain_80.txt")
    );
}

#[test]
fn top_level_help_matches_golden_at_narrow_and_wide_widths() {
    assert_eq!(
        top_level(60, HelpStyle::Plain),
        include_str!("golden/help_plain_60.txt")
    );
    assert_eq!(
        top_level(120, HelpStyle::Plain),
        include_str!("golden/help_plain_120.txt")
    );
}

#[test]
fn top_level_help_matches_golden_with_terminal_styles() {
    assert_eq!(
        top_level(TOP_LEVEL_HELP_DEFAULT_WIDTH, HelpStyle::Ansi),
        include_str!("golden/help_ansi_80.txt")
    );
}

#[test]
fn zero_columns_render_at_the_default_width() {
    assert_eq!(
        top_level(0, HelpStyle::Plain),
        top_level(TOP_LEVEL_HELP_DEFAULT_WIDTH, HelpStyle::Plain)
    );
}

#[test]
fn top_level_help_aliases_render_the_same_accurate_navigation_page() {
    let stdout = top_level(TOP_LEVEL_HELP_DEFAULT_WIDTH, HelpStyle::Plain);
    assert!(!stdout.contains("\x1b["));
    assert!(!stdout.contains("\x1b]2;"));
    assert!(stdout.starts_with("oh-fx v9.8.7\nFast, native coding agent for the terminal.\n"));
    for expected in [
        "oh-fx starts an interactive session by default.",
        "Commands:\n",
        "Run one noninteractive request",
        "Sign in to a model provider",
        "Sign out of a model provider",
        "Choose the active model provider",
        "Configure a Vercel AI Gateway API key",
        "Choose a Vercel AI Gateway team",
        "Show Vercel AI Gateway credits",
        "credits|balance",
        "Flags:\n",
        "--context-limit <spec>",
        "Set name=bytes|off; repeatable",
        "--add-dir <path>",
        "-c, --continue",
        "-r",
        "Open the saved-session picker",
        "--resume [last|<id>]",
        "--resume-last",
        "session resume [last|id]",
        "-v, --version",
        "Print the oh-fx version and exit",
        "Examples:\n",
        "https://github.com/binbandit/oh-fx",
        "run `/feedback` inside oh-fx",
        "Run `oh-fx <command> --help` for command-specific usage and options.",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}");
    }
    for unexpected in [
        "Sign in to Vercel or a selected provider",
        "-c, -r, --continue",
        "Must appear before the command",
        "command-specific options and examples",
        "  Work      ",
        "\n\n\nRun `oh-fx <command> --help`",
    ] {
        assert!(!stdout.contains(unexpected), "unexpected {unexpected:?}");
    }
}

#[test]
fn ask_help_renders_documented_options() {
    let expected = "oh-fx ask

Run one noninteractive request

Usage:
  oh-fx ask [--auto|--full-access] [--model <id>] [--effort <level>] [--fast|--no-fast] [--ultrafast|--no-ultrafast] [--provider-order <a,b,...>] [--provider-strict|--no-provider-strict] [--image PATH] [--system TEXT] [--json] [--quiet] [--prompt-permissions] [--no-save] [--sessions-v2] [--no-color] [--resume <last|id>|--resume-id <id>] [--continue-recovery] [--] <prompt>

Options:
  --auto                      Automatically review unresolved permission requests
  --full-access               Disable oh-fx permission checks
  --yolo                      Alias for --full-access
  --model <id>                Override the model for this request
  --effort <level>            Override the reasoning effort for this request
  --fast                      Enable Fast mode for this request when the model supports it
  --no-fast                   Disable Fast mode for this request
  --ultrafast                 Request Ultra mode for this request when the model supports it
  --no-ultrafast              Disable Ultra mode for this request
  --provider-order <a,b,...>  Prefer these gateway providers in order for this request
  --provider-strict           Restrict this request to only the providers in --provider-order
  --no-provider-strict        Clear the provider restriction for this request
  --image PATH                Attach an image file; repeat for multiple images
  --system TEXT               Replace the built-in system prompt for this request
  --json                      Emit machine-readable JSON instead of text
  --quiet                     Suppress assistant output
  --prompt-permissions        Prompt for Y/N permission approval when stdin is a TTY
  --no-save                   Do not save the session; incompatible with --resume and --resume-id
  --sessions-v2               Use the experimental v2 session store, also set by OH_FX_SESSIONS_V2=1; its sessions resume only with it
  --no-color                  Render TTY output without colors or hyperlinks
  --resume <last|id>          Continue the last session or a session by id
  --resume-id <id>            Continue a session by exact id
  --continue-recovery         Resume the paused model response in the selected session
  --                          Treat every following argument as prompt text

The prompt may be passed as arguments or piped on stdin when no prompt args are given.
TTY stdout uses the Minimal transcript presentation; redirected stdout emits raw assistant Markdown.
Operational progress and diagnostics are written to stderr. JSON `output` keeps accumulated assistant Markdown; `final_output` contains only the completed final response, or an empty string when absent.
JSON usage sums reported main-agent input_tokens and output_tokens, including with --no-save; unreported counts are null. Nested usage and dollar spend are excluded.
--system replaces only the built-in base prompt for this request; tool, skill, project, and runtime context still apply.
With --prompt-permissions, JSON and quiet requests may prompt on stderr only when stdin is a TTY.
";
    assert_eq!(command(TopLevelKind::Ask), expected);
}

#[test]
fn session_help_documents_inspect_resume_migrate_and_recover() {
    let text = command(TopLevelKind::Session);
    for expected in [
        "Inspect, resume, migrate, or recover saved sessions",
        "session <last|id>|--id <id>",
        "session resume [last|<id>]",
        "session migrate <id>|--id <id>",
        "session recover <id>|--id <id>",
    ] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
}

#[test]
fn acp_help_documents_accepted_options() {
    let text = command(TopLevelKind::Acp);
    assert!(text.contains(
        "Usage:\n  oh-fx acp [--model <id>] [--ultrafast|--no-ultrafast] [--log-file <path>]"
    ));
    assert!(text.contains("--model <id>"));
    assert!(text.contains("--log-file <path>"));
}

#[test]
fn replay_help_describes_golden_output() {
    let text = command(TopLevelKind::Replay);
    assert!(text.contains("--golden <path>"));
    assert!(text.contains("Write the final rendered grid to a file"));
    assert!(!text.contains("Compare output against a golden file"));
}

#[test]
fn upgrade_help_documents_the_single_release_stream() {
    let text = command(TopLevelKind::Upgrade);
    assert!(text.contains("Usage:\n  oh-fx upgrade [--json]\n"));
    assert!(!text.contains("--channel"));
}

#[test]
fn mcp_help_lists_every_subcommand() {
    let text = command(TopLevelKind::Mcp);
    for expected in [
        "oh-fx mcp add NAME COMMAND [ARGS...]",
        "oh-fx mcp auth NAME",
        "oh-fx mcp list",
        "oh-fx mcp logout NAME",
        "oh-fx mcp path",
        "oh-fx mcp remove NAME",
        "oh-fx mcp trust approve|reject NAME",
        "oh-fx mcp trust approve-all|reset",
    ] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
}

#[test]
fn help_respects_sixty_columns() {
    let text = top_level(60, HelpStyle::Plain);
    for expected in ["Commands:", "ask", "setup", "status", "doctor"] {
        assert!(text.contains(expected));
    }
    assert!(text.lines().all(|line| line.chars().count() <= 60));
}

#[test]
fn narrow_help_stacks_rows_so_only_unbreakable_words_overflow() {
    for columns in 20..60 {
        let text = top_level(columns, HelpStyle::Plain);
        for line in text.lines().filter(|line| line.chars().count() > columns) {
            assert!(!line.trim_start().contains(' '), "{columns}: {line:?}");
        }
    }
    let text = top_level(40, HelpStyle::Plain);
    assert!(text.contains("\n  ask <prompt>\n      Run one noninteractive request\n"));
    assert!(text.contains("\nLearn more about oh-fx:\n  https://github.com/binbandit/oh-fx\n"));
}

#[test]
fn help_hides_developer_recording_surfaces() {
    let text = top_level(TOP_LEVEL_HELP_DEFAULT_WIDTH, HelpStyle::Plain);
    assert!(!text.contains("--record"));
    assert!(!text.contains("replay <tape>"));
}
