use crate::command_specs::{
    SlashKind, SlashPresentationCategory, SlashSpec, SourceOptionDoc, SourceTopLevelSpec,
    TopLevelExample, TopLevelFlag, TopLevelHelp, TopLevelHelpEntry, TopLevelKind, TopLevelResource,
};
use crate::registry::SlashRegistry;

pub(crate) use crate::command_specs::packed_top_level::TOP_LEVEL_SPECS;

const JSON_OPTION: SourceOptionDoc =
    SourceOptionDoc::new("--json", "Emit machine-readable JSON instead of text");

pub(crate) const TOP_LEVEL_SOURCES: &[SourceTopLevelSpec] = &[
    SourceTopLevelSpec::new(TopLevelKind::Help, "help", "help", "Show this help")
        .with_aliases(&["--help", "-h"]),
    SourceTopLevelSpec::new(
        TopLevelKind::Ask,
        "ask",
        "ask [--auto|--full-access] [--model <id>] [--effort <level>] [--fast|--no-fast] [--ultrafast|--no-ultrafast] [--provider-order <a,b,...>] [--provider-strict|--no-provider-strict] [--image PATH] [--system TEXT] [--json] [--quiet] [--prompt-permissions] [--no-save] [--sessions-v2] [--no-color] [--resume <last|id>|--resume-id <id>] [--continue-recovery] [--] <prompt>",
        "Run one noninteractive request",
    )
    .with_options(&[
        SourceOptionDoc::new("--auto", "Automatically review unresolved permission requests"),
        SourceOptionDoc::new("--full-access", "Disable oh-fx permission checks"),
        SourceOptionDoc::new("--yolo", "Alias for --full-access"),
        SourceOptionDoc::new("--model <id>", "Override the model for this request"),
        SourceOptionDoc::new("--effort <level>", "Override the reasoning effort for this request"),
        SourceOptionDoc::new("--fast", "Enable Fast mode for this request when the model supports it"),
        SourceOptionDoc::new("--no-fast", "Disable Fast mode for this request"),
        SourceOptionDoc::new(
            "--ultrafast",
            "Request Ultra mode for this request when the model supports it",
        ),
        SourceOptionDoc::new("--no-ultrafast", "Disable Ultra mode for this request"),
        SourceOptionDoc::new(
            "--provider-order <a,b,...>",
            "Prefer these gateway providers in order for this request",
        ),
        SourceOptionDoc::new(
            "--provider-strict",
            "Restrict this request to only the providers in --provider-order",
        ),
        SourceOptionDoc::new(
            "--no-provider-strict",
            "Clear the provider restriction for this request",
        ),
        SourceOptionDoc::new("--image PATH", "Attach an image file; repeat for multiple images"),
        SourceOptionDoc::new("--system TEXT", "Replace the built-in system prompt for this request"),
        JSON_OPTION,
        SourceOptionDoc::new("--quiet", "Suppress assistant output"),
        SourceOptionDoc::new(
            "--prompt-permissions",
            "Prompt for Y/N permission approval when stdin is a TTY",
        ),
        SourceOptionDoc::new(
            "--no-save",
            "Do not save the session; incompatible with --resume and --resume-id",
        ),
        SourceOptionDoc::new(
            "--sessions-v2",
            "Use the experimental v2 session store, also set by OH_FX_SESSIONS_V2=1; its sessions resume only with it",
        ),
        SourceOptionDoc::new("--no-color", "Render TTY output without colors or hyperlinks"),
        SourceOptionDoc::new("--resume <last|id>", "Continue the last session or a session by id"),
        SourceOptionDoc::new("--resume-id <id>", "Continue a session by exact id"),
        SourceOptionDoc::new(
            "--continue-recovery",
            "Resume the paused model response in the selected session",
        ),
        SourceOptionDoc::new("--", "Treat every following argument as prompt text"),
    ])
    .with_details(&[
        "The prompt may be passed as arguments or piped on stdin when no prompt args are given.",
        "TTY stdout uses the Minimal transcript presentation; redirected stdout emits raw assistant Markdown.",
        "Operational progress and diagnostics are written to stderr. JSON `output` keeps accumulated assistant Markdown; `final_output` contains only the completed final response, or an empty string when absent.",
        "JSON usage sums reported main-agent input_tokens and output_tokens, including with --no-save; unreported counts are null. Nested usage and dollar spend are excluded.",
        "--system replaces only the built-in base prompt for this request; tool, skill, project, and runtime context still apply.",
        "With --prompt-permissions, JSON and quiet requests may prompt on stderr only when stdin is a TTY.",
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Acp,
        "acp",
        "acp [--model <id>] [--ultrafast|--no-ultrafast] [--log-file <path>]",
        "Start an ACP server over stdio",
    )
    .with_options(&[
        SourceOptionDoc::new("--model <id>", "Override the default model"),
        SourceOptionDoc::new("--ultrafast", "Request Ultra mode when the model supports it"),
        SourceOptionDoc::new("--no-ultrafast", "Disable Ultra mode"),
        SourceOptionDoc::new("--log-file <path>", "Write ACP logs to a file"),
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Pr,
        "pr",
        "pr [--auto] [--create] [context]",
        "Draft or publish a pull request",
    )
    .with_options(&[
        SourceOptionDoc::new("--auto", "Automatically review unresolved permission requests"),
        SourceOptionDoc::new("--create", "Publish the drafted pull request via the GitHub CLI"),
    ])
    .with_details(&[
        "Must run inside a git repository. Without --create, the drafted PR is printed only.",
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Issue,
        "issue",
        "issue [--auto] [--create] [context]",
        "Draft or publish a GitHub issue",
    )
    .with_options(&[
        SourceOptionDoc::new("--auto", "Automatically review unresolved permission requests"),
        SourceOptionDoc::new("--create", "Publish the drafted issue via the GitHub CLI"),
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Login,
        "login",
        "login [vercel|codex|grok]",
        "Sign in to Vercel or a selected provider",
    ),
    SourceTopLevelSpec::new(
        TopLevelKind::Logout,
        "logout",
        "logout [vercel|codex|grok]",
        "Sign out of Vercel or a selected provider session",
    ),
    SourceTopLevelSpec::new(
        TopLevelKind::Setup,
        "setup",
        "setup",
        "Configure an AI Gateway API key",
    ),
    SourceTopLevelSpec::new(
        TopLevelKind::Status,
        "status",
        "status [--json]",
        "Show configuration and runtime information",
    )
    .with_options(&[JSON_OPTION]),
    SourceTopLevelSpec::new(
        TopLevelKind::Permissions,
        "permissions",
        "permissions [--json]",
        "Show the permission mode and rules",
    )
    .with_options(&[JSON_OPTION])
    .with_details(&[
        "Modes:",
        "  ask          Prompt before sensitive tool calls",
        "  auto         Apply rules, then review unresolved sensitive tool calls (default)",
        "  full-access  Disable oh-fx permission checks",
        "",
        "Change the mode from the interactive shell with `/permissions [ask|auto|full-access|reset]`,",
        "and manage persistent allow rules with `/allowlist`.",
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Mcp,
        "mcp",
        "mcp <command> ...",
        "Manage MCP servers without opening the interactive shell",
    )
    .with_details(&[
        "Commands:",
        "  oh-fx mcp add NAME COMMAND [ARGS...]",
        "  oh-fx mcp add --transport http NAME URL",
        "  oh-fx mcp auth NAME",
        "  oh-fx mcp list [--connect]",
        "  oh-fx mcp logout NAME",
        "  oh-fx mcp path",
        "  oh-fx mcp remove NAME",
        "  oh-fx mcp trust approve|reject NAME",
        "  oh-fx mcp trust approve-all|reset",
        "",
        "By default, list reads configuration without opening MCP transports.",
        "Use --connect to connect and discover servers before rendering health.",
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Models,
        "models",
        "models [--json]",
        "List available models",
    )
    .with_options(&[JSON_OPTION]),
    SourceTopLevelSpec::new(
        TopLevelKind::Provider,
        "provider",
        "provider <name>",
        "Choose the model provider used by oh-fx",
    ),
    SourceTopLevelSpec::new(
        TopLevelKind::Doctor,
        "doctor",
        "doctor [--json]",
        "Run local health and preflight checks",
    )
    .with_options(&[JSON_OPTION]),
    SourceTopLevelSpec::new(
        TopLevelKind::Teams,
        "teams",
        "teams",
        "Choose the Vercel team used by AI Gateway",
    ),
    SourceTopLevelSpec::new(
        TopLevelKind::Session,
        "session",
        "session <last|id>|--id <id> [--json] | session resume [last|<id>] | session resume --id <id> | session migrate <id>|--id <id> [--allow-large] [--json] | session recover <id>|--id <id> [--json]",
        "Inspect, resume, migrate, or recover saved sessions",
    )
    .with_options(&[
        SourceOptionDoc::new("last", "Inspect the current workspace session"),
        SourceOptionDoc::new("--id <id>", "Inspect a saved session by exact id"),
        SourceOptionDoc::new(
            "resume [last|<id>]",
            "Resume the latest workspace session or a session by id",
        ),
        SourceOptionDoc::new("migrate <id>", "Migrate a saved session to the current format"),
        SourceOptionDoc::new(
            "recover <id>",
            "Copy a recoverable corrupt session into a new session",
        ),
        SourceOptionDoc::new("--allow-large", "Permit migrating an oversized session"),
        JSON_OPTION,
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Sessions,
        "sessions",
        "sessions [--all] [--limit <1-100>] [--cursor <cursor>] [--json]",
        "List saved sessions for the current workspace",
    )
    .with_options(&[
        SourceOptionDoc::new(
            "--all",
            "List saved sessions across every workspace in this profile",
        ),
        SourceOptionDoc::new("--limit <1-100>", "Set the maximum sessions returned per page"),
        SourceOptionDoc::new("--cursor <cursor>", "Continue from a prior sessions result"),
        JSON_OPTION,
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Resume,
        "resume",
        "session resume [last|<id>] | session resume --id <id> | --resume [last|<id>] | resume [last|<id>] | resume --id <id> | --resume-last | --continue | -c | -r | --resume-<id>",
        "Continue a saved interactive session",
    )
    .with_aliases(&["--resume", "--resume-last", "--continue", "-c", "-r"])
    .hidden()
    .with_options(&[
        SourceOptionDoc::new("-r", "Choose the session to resume from a picker"),
        SourceOptionDoc::new("last", "Resume the most recent session"),
        SourceOptionDoc::new("<id>", "Resume a session by id"),
        SourceOptionDoc::new("--id <id>", "Resume a session by exact id"),
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Credits,
        "credits",
        "credits [--json]",
        "Show the AI Gateway credit balance",
    )
    .with_aliases(&["balance"])
    .with_options(&[JSON_OPTION]),
    SourceTopLevelSpec::new(
        TopLevelKind::Usage,
        "usage",
        "usage [--period <24h|7d|30d>] [--json]",
        "Show local oh-fx token usage and spend",
    )
    .with_options(&[
        SourceOptionDoc::new(
            "--period <24h|7d|30d>",
            "Select a rolling window (default: 30d)",
        ),
        JSON_OPTION,
    ])
    .with_details(&[
        "Reports only usage recorded by oh-fx on this machine.",
        "This command reads local state and does not query account-wide Gateway reports.",
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Upgrade,
        "upgrade",
        "upgrade [--json]",
        "Upgrade oh-fx to the latest release",
    )
    .with_options(&[JSON_OPTION]),
    SourceTopLevelSpec::new(
        TopLevelKind::Replay,
        "replay",
        "replay <tape> [--frames] [--json] [--golden <path>] [--frames-dir <path>]",
        "Replay a recorded terminal session",
    )
    .hidden()
    .with_options(&[
        SourceOptionDoc::new("--frames", "Render each captured frame"),
        SourceOptionDoc::new("--golden <path>", "Write the final rendered grid to a file"),
        SourceOptionDoc::new("--frames-dir <path>", "Write rendered frames to a directory"),
        JSON_OPTION,
    ]),
    SourceTopLevelSpec::new(
        TopLevelKind::Workspace,
        "workspace",
        "workspace [list|add PATH|remove PATH|clear] [--json]",
        "Manage additional workspace directories",
    )
    .with_options(&[
        SourceOptionDoc::new(
            "list",
            "List the primary and additional directories (default)",
        ),
        SourceOptionDoc::new("add PATH", "Persist an existing additional directory"),
        SourceOptionDoc::new("remove PATH", "Remove an additional directory"),
        SourceOptionDoc::new("clear", "Remove all additional directories"),
        JSON_OPTION,
    ])
    .with_details(&["Additional directories are stored for the current primary workspace."]),
];

const TOP_LEVEL_HELP_GROUPS: &[&[TopLevelHelpEntry]] = &[
    &[TopLevelHelpEntry::command(
        TopLevelKind::Ask,
        "ask <prompt>",
    )],
    &[
        TopLevelHelpEntry::command(TopLevelKind::Pr, "pr [context]"),
        TopLevelHelpEntry::command(TopLevelKind::Issue, "issue [context]"),
    ],
    &[
        TopLevelHelpEntry::command(TopLevelKind::Sessions, "sessions"),
        TopLevelHelpEntry::command(TopLevelKind::Session, "session <last|id>"),
        TopLevelHelpEntry::extra(
            "session resume [last|id]",
            "Resume the latest workspace session or a session by id",
        ),
        TopLevelHelpEntry::extra(
            "session migrate <id>",
            "Migrate a saved session to the current format",
        ),
        TopLevelHelpEntry::extra("session recover <id>", "Copy a recoverable corrupt session"),
    ],
    &[
        TopLevelHelpEntry::summarized(
            TopLevelKind::Login,
            "login [vercel|codex|grok]",
            "Sign in to a model provider",
        ),
        TopLevelHelpEntry::summarized(
            TopLevelKind::Logout,
            "logout [vercel|codex|grok]",
            "Sign out of a model provider",
        ),
        TopLevelHelpEntry::summarized(
            TopLevelKind::Provider,
            "provider <name>",
            "Choose the active model provider",
        ),
        TopLevelHelpEntry::command(TopLevelKind::Models, "models"),
    ],
    &[
        TopLevelHelpEntry::summarized(
            TopLevelKind::Setup,
            "setup",
            "Configure a Vercel AI Gateway API key",
        ),
        TopLevelHelpEntry::summarized(
            TopLevelKind::Teams,
            "teams",
            "Choose a Vercel AI Gateway team",
        ),
        TopLevelHelpEntry::summarized(
            TopLevelKind::Credits,
            "credits|balance",
            "Show Vercel AI Gateway credits",
        ),
    ],
    &[TopLevelHelpEntry::summarized(
        TopLevelKind::Usage,
        "usage [--period <24h|7d|30d>]",
        "Show locally recorded token usage and spend",
    )],
    &[
        TopLevelHelpEntry::command(TopLevelKind::Status, "status"),
        TopLevelHelpEntry::command(TopLevelKind::Doctor, "doctor"),
        TopLevelHelpEntry::command(TopLevelKind::Mcp, "mcp <command> ..."),
        TopLevelHelpEntry::command(TopLevelKind::Permissions, "permissions"),
        TopLevelHelpEntry::command(TopLevelKind::Workspace, "workspace"),
        TopLevelHelpEntry::command(TopLevelKind::Upgrade, "upgrade"),
        TopLevelHelpEntry::command(TopLevelKind::Acp, "acp"),
        TopLevelHelpEntry::command(TopLevelKind::Help, "help"),
    ],
];

const TOP_LEVEL_FLAGS: &[TopLevelFlag] = &[
    TopLevelFlag {
        usage: "--context-limit <spec>",
        description: "Set name=bytes|off; repeatable",
    },
    TopLevelFlag {
        usage: "--add-dir <path>",
        description: "Add a workspace directory; repeatable",
    },
    TopLevelFlag {
        usage: "--no-additional-dirs",
        description: "Ignore saved additional directories",
    },
    TopLevelFlag {
        usage: "--provider <name>",
        description: "Override the model provider for an interactive session (gateway, codex, grok, or a configured name)",
    },
    TopLevelFlag {
        usage: "--model <id>",
        description: "Override the model for an interactive session",
    },
    TopLevelFlag {
        usage: "--effort <level>",
        description: "Override the reasoning effort for an interactive session",
    },
    TopLevelFlag {
        usage: "--fast, --no-fast",
        description: "Turn Fast mode on or off for an interactive session",
    },
    TopLevelFlag {
        usage: "--ultrafast, --no-ultrafast",
        description: "Request Ultra mode on or off for an interactive session",
    },
    TopLevelFlag {
        usage: "-c, --continue",
        description: "Resume the remembered workspace session",
    },
    TopLevelFlag {
        usage: "-r",
        description: "Open the saved-session picker",
    },
    TopLevelFlag {
        usage: "--resume [last|<id>]",
        description: "Resume the latest workspace session or an exact ID",
    },
    TopLevelFlag {
        usage: "--resume-last",
        description: "Resume the latest workspace session",
    },
    TopLevelFlag {
        usage: "--resume-<id>",
        description: "Resume a session by exact ID",
    },
    TopLevelFlag {
        usage: "--sessions-v2",
        description: "Use the experimental v2 session store, also set by OH_FX_SESSIONS_V2=1",
    },
    TopLevelFlag {
        usage: "-h, --help",
        description: "Display this help and exit",
    },
    TopLevelFlag {
        usage: "-v, --version",
        description: "Print the oh-fx version and exit",
    },
];

const TOP_LEVEL_EXAMPLES: &[TopLevelExample] = &[
    TopLevelExample {
        command: "oh-fx",
        description: "Start a fresh interactive session",
    },
    TopLevelExample {
        command: "oh-fx ask \"Explain the changes in this repository\"",
        description: "Run one request and exit",
    },
    TopLevelExample {
        command: "oh-fx session resume last",
        description: "Continue the latest session for this workspace",
    },
    TopLevelExample {
        command: "oh-fx status --json",
        description: "Inspect the current configuration as JSON",
    },
];

const TOP_LEVEL_NOTES: &[&str] = &[
    "Run `oh-fx <command> --help` for command-specific usage and options.",
    "Run `/help` inside an interactive session for slash commands.",
];

const TOP_LEVEL_RESOURCES: &[TopLevelResource] = &[
    TopLevelResource {
        label: "Learn more about oh-fx:",
        value: "https://github.com/binbandit/oh-fx",
        link: true,
    },
    TopLevelResource {
        label: "Report a problem:",
        value: "run `/feedback` inside oh-fx",
        link: false,
    },
];

pub(crate) static TOP_LEVEL_HELP: TopLevelHelp = TopLevelHelp {
    description: "Fast, native coding agent for the terminal.",
    interactive_hint: "oh-fx starts an interactive session by default. Use `oh-fx ask` to run one noninteractive request.",
    help_groups: TOP_LEVEL_HELP_GROUPS,
    flags: TOP_LEVEL_FLAGS,
    examples: TOP_LEVEL_EXAMPLES,
    notes: TOP_LEVEL_NOTES,
    resources: TOP_LEVEL_RESOURCES,
};

pub(crate) const SLASH_SPECS: &[SlashSpec] = &[
    SlashSpec::new(
        SlashKind::Help,
        "/help",
        "show available slash commands",
        SlashPresentationCategory::General,
    ),
    SlashSpec::new(
        SlashKind::ClearScreen,
        "/clear",
        "start a fresh conversation while keeping managed processes",
        SlashPresentationCategory::General,
    ),
    SlashSpec::new(
        SlashKind::NewSession,
        "/new",
        "start a fresh session",
        SlashPresentationCategory::Session,
    ),
    SlashSpec::new(
        SlashKind::ResetSession,
        "/reset",
        "reset the current session context",
        SlashPresentationCategory::Session,
    ),
    SlashSpec::new(
        SlashKind::ResumeSession,
        "/resume",
        "resume a saved session",
        SlashPresentationCategory::Session,
    ),
    SlashSpec::new(
        SlashKind::RenameSession,
        "/rename",
        "rename the current session",
        SlashPresentationCategory::Session,
    )
    .with_payload()
    .with_help("/rename <title>"),
    SlashSpec::new(
        SlashKind::Login,
        "/login",
        "choose the model provider and how it signs in",
        SlashPresentationCategory::Account,
    ),
    SlashSpec::new(
        SlashKind::Logout,
        "/logout",
        "sign out of a provider session",
        SlashPresentationCategory::Account,
    )
    .with_payload()
    .with_help("/logout [vercel|codex|grok]"),
    SlashSpec::new(
        SlashKind::Provider,
        "/provider",
        "choose the model provider and how it signs in",
        SlashPresentationCategory::Account,
    )
    .with_aliases(&["/setup"]),
    SlashSpec::new(
        SlashKind::Stats,
        "/stats",
        "show token and turn statistics",
        SlashPresentationCategory::Account,
    ),
    SlashSpec::new(
        SlashKind::Usage,
        "/usage",
        "show local oh-fx tokens, models, and spend",
        SlashPresentationCategory::Account,
    )
    .with_aliases(&["/cost"])
    .with_help("/usage (/cost)"),
    SlashSpec::new(
        SlashKind::Status,
        "/status",
        "show runtime configuration",
        SlashPresentationCategory::General,
    ),
    SlashSpec::new(
        SlashKind::Model,
        "/model",
        "choose what model and reasoning effort to use",
        SlashPresentationCategory::Model,
    )
    .with_payload()
    .with_help("/model <id-or-query>"),
    SlashSpec::new(
        SlashKind::Permissions,
        "/permissions",
        "choose what oh-fx is allowed to do",
        SlashPresentationCategory::Security,
    )
    .with_payload()
    .with_help("/permissions [ask|auto|full-access|reset]"),
    SlashSpec::new(
        SlashKind::Allowlist,
        "/allowlist",
        "manage trusted commands, tools, and URLs",
        SlashPresentationCategory::Security,
    )
    .with_payload()
    .with_help("/allowlist [view [effective|local|user]|[local|user] add|remove|reset ...]"),
    SlashSpec::new(
        SlashKind::Undo,
        "/undo",
        "undo the latest tracked file operation",
        SlashPresentationCategory::Session,
    ),
    SlashSpec::new(
        SlashKind::Mcp,
        "/mcp",
        "manage local and remote MCP servers, resources, prompts, and project trust",
        SlashPresentationCategory::Extensions,
    )
    .with_payload()
    .with_help("/mcp [list|resource|prompt|add|remove|path|reload|auth|logout|trust]"),
    SlashSpec::new(
        SlashKind::Skills,
        "/skills",
        "browse and manage skills",
        SlashPresentationCategory::Extensions,
    )
    .with_payload()
    .with_help(
        "/skills [list|add|install|show|create|remove|path] [name|url|path] ($ opens skill search)",
    ),
    SlashSpec::new(
        SlashKind::Copy,
        "/copy",
        "copy the last assistant response",
        SlashPresentationCategory::Session,
    ),
    SlashSpec::new(
        SlashKind::Feedback,
        "/feedback",
        "open the oh-fx issue form",
        SlashPresentationCategory::Product,
    ),
    SlashSpec::new(
        SlashKind::Trace,
        "/trace",
        "copy a private diagnostic trace",
        SlashPresentationCategory::Product,
    ),
    SlashSpec::new(
        SlashKind::Compact,
        "/compact",
        "summarize context into a fresh window",
        SlashPresentationCategory::Session,
    ),
    SlashSpec::new(
        SlashKind::Settings,
        "/settings",
        "browse and update settings",
        SlashPresentationCategory::Appearance,
    )
    .with_payload()
    .with_help("/settings [startup-scrollback [on|off]]"),
    SlashSpec::new(
        SlashKind::Alias,
        "/alias",
        "show alias availability",
        SlashPresentationCategory::Extensions,
    )
    .with_payload()
    .with_help("/alias [name] [command]"),
    SlashSpec::new(
        SlashKind::Fast,
        "/fast",
        "toggle Fast mode when supported",
        SlashPresentationCategory::Model,
    ),
    SlashSpec::new(
        SlashKind::Ultrafast,
        "/ultrafast",
        "request Ultra mode when supported",
        SlashPresentationCategory::Model,
    )
    .with_payload()
    .with_help("/ultrafast [on|off|status]"),
    SlashSpec::new(
        SlashKind::Statusline,
        "/statusline",
        "toggle status line segments",
        SlashPresentationCategory::Appearance,
    )
    .with_payload()
    .with_help("/statusline [context|session|workspace]"),
    SlashSpec::new(
        SlashKind::Workspace,
        "/workspace",
        "manage additional workspace directories",
        SlashPresentationCategory::Workspace,
    )
    .with_payload()
    .with_help("/workspace [list|add PATH|remove PATH|clear]"),
    SlashSpec::new(
        SlashKind::Shell,
        "/shell",
        "reload shell startup files for commands",
        SlashPresentationCategory::Workspace,
    )
    .with_payload()
    .with_help("/shell reload"),
    SlashSpec::new(
        SlashKind::Version,
        "/version",
        "show the oh-fx version",
        SlashPresentationCategory::General,
    ),
    SlashSpec::new(
        SlashKind::Quit,
        "/quit",
        "exit the interactive shell",
        SlashPresentationCategory::General,
    )
    .with_aliases(&["/exit"]),
];

pub static SLASH_REGISTRY: SlashRegistry<'static> = SlashRegistry::new(SLASH_SPECS);
