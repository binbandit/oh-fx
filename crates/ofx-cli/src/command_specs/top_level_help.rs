use std::fmt::Write as _;

use ofx_text::parse_unsigned;

use super::{
    PRODUCT_NAME, TopLevelExample, TopLevelFlag, TopLevelHelp, TopLevelHelpEntry, TopLevelKind,
    TopLevelResource,
};
use crate::commands::TOP_LEVEL_HELP;

pub const TOP_LEVEL_HELP_DEFAULT_WIDTH: usize = 80;
const MIN_SUMMARY_COLUMNS: usize = 20;

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum HelpStyle {
    Plain,
    Ansi,
}

impl HelpStyle {
    pub fn for_terminal(is_terminal: bool, no_color: bool, dumb_terminal: bool) -> Self {
        if is_terminal && !no_color && !dumb_terminal {
            Self::Ansi
        } else {
            Self::Plain
        }
    }

    fn start(self, role: HelpRole) -> &'static str {
        match self {
            Self::Plain => "",
            Self::Ansi => role.escape(),
        }
    }

    fn end(self) -> &'static str {
        match self {
            Self::Plain => "",
            Self::Ansi => "\x1b[0m",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum HelpRole {
    Brand,
    Heading,
    Syntax,
    Muted,
    Label,
    Link,
}

impl HelpRole {
    fn escape(self) -> &'static str {
        match self {
            Self::Brand | Self::Heading | Self::Label => "\x1b[1m",
            Self::Syntax => "\x1b[39m",
            Self::Muted => "\x1b[38;5;243m",
            Self::Link => "\x1b[4m",
        }
    }
}

pub fn parse_column_count(value: &str) -> Option<usize> {
    parse_unsigned(value.trim_matches([' ', '\t'])).filter(|columns| *columns != 0)
}

pub fn render_top_level_help(columns: usize, version: &str, style: HelpStyle) -> String {
    let page = &TOP_LEVEL_HELP;
    let mut help = HelpWriter {
        out: String::new(),
        columns: if columns == 0 {
            TOP_LEVEL_HELP_DEFAULT_WIDTH
        } else {
            columns
        },
        style,
    };
    help.write_header(page, version);
    help.write_commands(page.help_groups);
    help.write_flags(page.flags);
    help.write_examples(page.examples);
    for note in page.notes {
        help.styled_line("", "", note, HelpRole::Muted);
    }
    help.out.push('\n');
    help.write_resources(page.resources);
    help.out
}

pub fn render_command_help(kind: TopLevelKind) -> String {
    let spec = kind.spec();
    let mut out = format!(
        "{PRODUCT_NAME} {}\n\n{}\n\nUsage:\n  {PRODUCT_NAME} {}\n",
        spec.token, spec.summary, spec.usage
    );
    if !spec.options.is_empty() {
        let flag_width = spec
            .options
            .iter()
            .map(|option| option.flag.len())
            .max()
            .unwrap_or(0);
        out.push_str("\nOptions:\n");
        for option in spec.options {
            let _ = writeln!(
                out,
                "  {:<width$}{}",
                option.flag,
                option.description,
                width = flag_width + 2
            );
        }
    }
    if !spec.details.is_empty() {
        out.push('\n');
        for line in spec.details {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn visible_entries(
    groups: &'static [&'static [TopLevelHelpEntry]],
) -> impl Iterator<Item = &'static TopLevelHelpEntry> {
    groups
        .iter()
        .flat_map(|group| group.iter())
        .filter(|entry| !entry.is_hidden())
}

struct HelpWriter {
    out: String,
    columns: usize,
    style: HelpStyle,
}

impl HelpWriter {
    fn write_header(&mut self, page: &TopLevelHelp, version: &str) {
        self.styled(HelpRole::Brand, PRODUCT_NAME);
        self.out.push(' ');
        self.out.push_str(self.style.start(HelpRole::Muted));
        self.out.push('v');
        self.out.push_str(version);
        self.out.push_str(self.style.end());
        self.out.push('\n');
        self.styled_line("", "", page.description, HelpRole::Muted);
        self.out.push('\n');
        self.styled_line("", "", page.interactive_hint, HelpRole::Muted);
        self.section_heading("Usage:");
        let usages = [
            format!("{PRODUCT_NAME} [flags]"),
            format!("{PRODUCT_NAME} <command> [...flags] [...args]"),
        ];
        for usage in usages {
            self.styled_line("  ", "  ", &usage, HelpRole::Syntax);
        }
    }

    fn write_commands(&mut self, groups: &'static [&'static [TopLevelHelpEntry]]) {
        self.section_heading("Commands:");
        let usage_width = visible_entries(groups)
            .map(|entry| entry.usage().len())
            .max()
            .unwrap_or(0);
        for (index, group) in groups.iter().enumerate() {
            if index > 0 {
                self.out.push('\n');
            }
            for entry in group.iter().filter(|entry| !entry.is_hidden()) {
                self.entry(entry.usage(), usage_width, entry.summary());
            }
        }
    }

    fn write_flags(&mut self, flags: &[TopLevelFlag]) {
        self.section_heading("Flags:");
        let usage_width = flags.iter().map(|flag| flag.usage.len()).max().unwrap_or(0);
        for flag in flags {
            self.entry(flag.usage, usage_width, flag.description);
        }
        self.out.push('\n');
    }

    fn entry(&mut self, usage: &str, usage_width: usize, summary: &str) {
        if usage_width + 4 + MIN_SUMMARY_COLUMNS > self.columns {
            self.styled_line("  ", "  ", usage, HelpRole::Syntax);
            self.line("      ", "      ", summary, Decoration::NONE);
            return;
        }
        let prefix = self.padded_syntax(usage, usage_width);
        let continuation = " ".repeat(usage_width + 4);
        self.line(&prefix, &continuation, summary, Decoration::NONE);
    }

    fn write_examples(&mut self, examples: &[TopLevelExample]) {
        self.styled(HelpRole::Heading, "Examples:");
        self.out.push('\n');
        for example in examples {
            self.styled_line("  ", "  ", example.command, HelpRole::Syntax);
            self.styled_line("      ", "      ", example.description, HelpRole::Muted);
            self.out.push('\n');
        }
    }

    fn write_resources(&mut self, resources: &[TopLevelResource]) {
        let label_width = resources
            .iter()
            .map(|resource| visible_width(resource.label))
            .max()
            .unwrap_or(0);
        let longest_word = resources
            .iter()
            .flat_map(|resource| resource.value.split(' '))
            .map(visible_width)
            .max()
            .unwrap_or(0);
        let stacked = label_width + 2 + longest_word.max(MIN_SUMMARY_COLUMNS) > self.columns;
        for resource in resources {
            let role = if resource.link {
                HelpRole::Link
            } else {
                HelpRole::Syntax
            };
            if stacked {
                self.styled_line("", "", resource.label, HelpRole::Label);
                self.styled_line("  ", "  ", resource.value, role);
                continue;
            }
            let padding = label_width - visible_width(resource.label) + 2;
            let prefix = format!(
                "{}{}{}{}",
                self.style.start(HelpRole::Label),
                resource.label,
                self.style.end(),
                " ".repeat(padding)
            );
            self.styled_line(&prefix, "  ", resource.value, role);
        }
    }

    fn padded_syntax(&self, usage: &str, width: usize) -> String {
        format!(
            "  {}{usage}{}{}",
            self.style.start(HelpRole::Syntax),
            self.style.end(),
            " ".repeat(width - usage.len() + 2)
        )
    }

    fn section_heading(&mut self, heading: &str) {
        self.out.push('\n');
        self.styled(HelpRole::Heading, heading);
        self.out.push('\n');
    }

    fn styled(&mut self, role: HelpRole, text: &str) {
        self.out.push_str(self.style.start(role));
        self.out.push_str(text);
        self.out.push_str(self.style.end());
    }

    fn styled_line(&mut self, prefix: &str, continuation: &str, text: &str, role: HelpRole) {
        let decoration = Decoration {
            start: self.style.start(role),
            end: self.style.end(),
        };
        self.line(prefix, continuation, text, decoration);
    }

    fn line(&mut self, prefix: &str, continuation: &str, text: &str, decoration: Decoration) {
        let mut budget = self.columns.saturating_sub(visible_width(prefix)).max(1);
        let continuation_budget = self
            .columns
            .saturating_sub(visible_width(continuation))
            .max(1);
        let mut line_width = 0;
        self.out.push_str(prefix);
        self.out.push_str(decoration.start);
        for word in text.split(' ').filter(|word| !word.is_empty()) {
            let word_width = visible_width(word);
            if line_width == 0 {
                line_width = word_width;
            } else if line_width + 1 + word_width <= budget {
                self.out.push(' ');
                line_width += 1 + word_width;
            } else {
                self.out.push_str(decoration.end);
                self.out.push('\n');
                self.out.push_str(continuation);
                self.out.push_str(decoration.start);
                budget = continuation_budget;
                line_width = word_width;
            }
            self.out.push_str(word);
        }
        self.out.push_str(decoration.end);
        self.out.push('\n');
    }
}

#[derive(Debug, Clone, Copy)]
struct Decoration {
    start: &'static str,
    end: &'static str,
}

impl Decoration {
    const NONE: Self = Self { start: "", end: "" };
}

fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            chars.find(char::is_ascii_alphabetic);
        } else {
            width += 1;
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{TOP_LEVEL_HELP, TOP_LEVEL_SPECS};

    const TOP_LEVEL_HELP_FAST_BUFFER_BYTES: usize = 32 * 1024;

    fn help_text(columns: usize) -> String {
        render_top_level_help(columns, "9.8.7", HelpStyle::Plain)
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(character) = chars.next() {
            if character == '\x1b' {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) && next != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(character);
        }
        out
    }

    fn line_contains_both(text: &str, first: &str, second: &str) -> bool {
        text.lines()
            .any(|line| line.contains(first) && line.contains(second))
    }

    fn assert_all_lines_fit(text: &str, columns: usize) {
        for line in text.lines() {
            assert!(visible_width(line) <= columns, "{line:?} exceeds {columns}");
        }
    }

    fn contains_command_token(text: &str, token: &str) -> bool {
        let is_boundary = |byte: u8| b" \t\n,:<>[]|".contains(&byte);
        text.match_indices(token).any(|(start, _)| {
            let end = start + token.len();
            let before = start == 0 || is_boundary(text.as_bytes()[start - 1]);
            let after = end == text.len() || is_boundary(text.as_bytes()[end]);
            before && after
        })
    }

    #[test]
    fn rendered_top_level_help_is_a_complete_cli_navigation_page() {
        let text = help_text(TOP_LEVEL_HELP_DEFAULT_WIDTH);
        assert!(text.starts_with("oh-fx v9.8.7\nFast, native coding agent for the terminal."));
        assert_eq!(text.matches("v9.8.7").count(), 1);
        for expected in [
            "oh-fx starts an interactive session by default.",
            "oh-fx <command> [...flags] [...args]",
            "Commands:",
            "ask <prompt>",
            "Run one noninteractive request",
            "Draft or publish a GitHub issue",
            "credits|balance",
            "Sign in to a model provider",
            "Sign out of a model provider",
            "Choose the active model provider",
            "Configure a Vercel AI Gateway API key",
            "Choose a Vercel AI Gateway team",
            "Show Vercel AI Gateway credits",
            "Flags:",
            "--context-limit <spec>",
            "Set name=bytes|off; repeatable",
            "--add-dir <path>",
            "-c, --continue",
            "-r",
            "--resume [last|<id>]",
            "--resume-last",
            "-v, --version",
            "Examples:",
            "oh-fx ask \"Explain the changes in this repository\"",
            "oh-fx session resume last",
            "session resume [last|id]",
            "oh-fx status --json",
            "Run `oh-fx <command> --help` for command-specific usage and options.",
            "Run `/help` inside an interactive session for slash commands.",
            "Learn more about oh-fx:  https://github.com/binbandit/oh-fx",
            "\nReport a problem:        run `/feedback` inside oh-fx\n",
        ] {
            assert!(text.contains(expected), "missing {expected:?}");
        }
        for unexpected in [
            "Sign in to Vercel or a selected provider",
            "-c, -r, --continue",
            "Must appear before the command. Accepted names:",
            "skill_description_bytes, skill_catalog_bytes",
            "FX_EXPERIMENTAL_WORKSPACE_ACCESS=1",
            "Supported for interactive, resume, ask, ACP, PR, and issue launches",
            "command-specific options and examples",
            "\n\n\nRun `oh-fx <command> --help`",
            "Start:",
            "  Work      ",
            "More:",
            "resume [last|<id>] [--record]",
            "session migrate <id>|--id <id>",
        ] {
            assert!(!text.contains(unexpected), "unexpected {unexpected:?}");
        }
    }

    #[test]
    fn top_level_help_summary_overrides_do_not_change_command_specific_help() {
        let login = render_command_help(TopLevelKind::Login);
        assert!(login.contains("Sign in to Vercel or a selected provider"));
        assert!(!login.contains("Sign in to a model provider"));
    }

    #[test]
    fn terminal_top_level_help_adds_styling_without_changing_visible_content() {
        let plain = help_text(TOP_LEVEL_HELP_DEFAULT_WIDTH);
        let terminal =
            render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, "9.8.7", HelpStyle::Ansi);

        assert!(!plain.contains("\x1b["));
        assert!(terminal.starts_with("\x1b[1moh-fx\x1b[0m"));
        assert!(terminal.contains("\x1b[1mUsage:\x1b[0m"));
        assert!(terminal.contains("\x1b[39mask <prompt>\x1b[0m"));
        assert!(terminal.contains("\x1b[38;5;243mFast, native coding agent"));
        assert!(terminal.contains("\x1b[4mhttps://github.com/binbandit/oh-fx\x1b[0m"));
        assert!(terminal.contains("\x1b[39mrun `/feedback` inside oh-fx\x1b[0m"));
        assert!(!terminal.contains("\x1b[38;5;252m"));
        assert!(!terminal.contains("\x1b[38;5;245m"));
        assert_eq!(strip_ansi(&terminal), plain);
    }

    #[test]
    fn top_level_help_renders_flags_as_compact_aligned_rows() {
        let wide = help_text(120);
        let narrow = help_text(60);

        assert!(line_contains_both(
            &wide,
            "--context-limit <spec>",
            "Set name=bytes|off; repeatable"
        ));
        assert!(line_contains_both(
            &wide,
            "--add-dir <path>",
            "Add a workspace directory; repeatable"
        ));
        assert!(line_contains_both(
            &wide,
            "-c, --continue",
            "Resume the remembered workspace session"
        ));
        assert!(line_contains_both(
            &wide,
            "-r",
            "Open the saved-session picker"
        ));
        assert!(line_contains_both(
            &wide,
            "--resume [last|<id>]",
            "Resume the latest workspace session or an exact ID"
        ));
        assert!(line_contains_both(
            &wide,
            "--resume-last",
            "Resume the latest workspace session"
        ));
        assert!(wide.contains("Print the oh-fx version and exit\n\nExamples:"));
        assert!(wide.contains("List available models\n\n  setup"));
        assert!(wide.contains("Show Vercel AI Gateway credits\n\n  usage"));
        assert_all_lines_fit(&narrow, 60);
    }

    #[test]
    fn top_level_help_hides_developer_recording_surfaces() {
        let text = help_text(120);
        assert!(!text.contains("--record"));
        assert!(!text.contains("replay <tape>"));

        let replay = render_command_help(TopLevelKind::Replay);
        assert!(replay.contains("oh-fx replay"));
    }

    #[test]
    fn default_top_level_help_styles_fit_the_startup_buffer() {
        for style in [HelpStyle::Plain, HelpStyle::Ansi] {
            let text = render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, "9.8.7", style);
            assert!(text.len() <= TOP_LEVEL_HELP_FAST_BUFFER_BYTES);
        }
    }

    #[test]
    fn per_command_help_renders_header_usage_options_and_details() {
        let text = render_command_help(TopLevelKind::Permissions);
        assert!(text.contains("oh-fx permissions\n"));
        assert!(text.contains("Usage:\n  oh-fx permissions [--json]"));
        assert!(text.contains("Options:"));
        assert!(text.contains("--json"));
        assert!(text.contains("Modes:"));
    }

    #[test]
    fn per_command_help_preserves_long_resume_usage_without_debug_recording() {
        let text = render_command_help(TopLevelKind::Resume);
        assert!(text.contains("Usage:\n  oh-fx session resume [last|<id>] | session resume --id <id> | --resume [last|<id>] | resume [last|<id>] | resume --id <id> | --resume-last | --continue | -c | -r | --resume-<id>"));
        assert!(text.contains("Options:"));
        assert!(!text.contains("--record"));
    }

    #[test]
    fn acp_help_documents_accepted_options() {
        let text = render_command_help(TopLevelKind::Acp);
        assert!(text.contains("oh-fx acp\n"));
        assert!(text.contains("Usage:\n  oh-fx acp [--model <id>] [--log-file <path>]"));
        assert!(text.contains("--model <id>"));
        assert!(text.contains("--log-file <path>"));
    }

    #[test]
    fn hidden_top_level_commands_do_not_reserve_help_usage_width() {
        let widest = |entries: &mut dyn Iterator<Item = &TopLevelHelpEntry>| {
            entries.map(|entry| entry.usage().len()).max().unwrap_or(0)
        };
        let all_width = widest(
            &mut TOP_LEVEL_HELP
                .help_groups
                .iter()
                .flat_map(|group| group.iter()),
        );
        let visible_width = widest(&mut visible_entries(TOP_LEVEL_HELP.help_groups));
        assert!(all_width >= visible_width);
        let text = help_text(TOP_LEVEL_HELP_DEFAULT_WIDTH);
        let ask_line = text
            .lines()
            .find(|line| line.starts_with("  ask <prompt>"))
            .unwrap();
        assert_eq!(ask_line.find("Run one").unwrap(), visible_width + 4);
    }

    #[test]
    fn top_level_help_renders_every_visible_command_token() {
        let text = help_text(TOP_LEVEL_HELP_DEFAULT_WIDTH);
        for spec in TOP_LEVEL_SPECS {
            if !spec.hidden_from_top_level_help {
                assert!(contains_command_token(&text, spec.token), "{}", spec.token);
            }
        }
        for note in TOP_LEVEL_HELP.notes {
            assert!(text.contains(note));
        }
    }

    #[test]
    fn top_level_help_lines_fit_representative_terminal_widths() {
        for width in [60, 80, 120] {
            assert_all_lines_fit(&help_text(width), width);
        }
    }

    #[test]
    fn terminal_help_styling_respects_terminal_capability_and_color_opt_outs() {
        assert_eq!(HelpStyle::for_terminal(true, false, false), HelpStyle::Ansi);
        assert_eq!(
            HelpStyle::for_terminal(false, false, false),
            HelpStyle::Plain
        );
        assert_eq!(HelpStyle::for_terminal(true, true, false), HelpStyle::Plain);
        assert_eq!(HelpStyle::for_terminal(true, false, true), HelpStyle::Plain);
    }

    #[test]
    fn visible_width_skips_the_help_styles() {
        assert_eq!(visible_width("\x1b[1mhello\x1b[22m"), 5);
        assert_eq!(visible_width("  \x1b[38;5;243mab\x1b[0m  "), 6);
        for style in [HelpStyle::Plain, HelpStyle::Ansi] {
            let text = render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, "9.8.7", style);
            let widths: Vec<usize> = text.lines().map(visible_width).collect();
            let plain =
                render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, "9.8.7", HelpStyle::Plain);
            assert_eq!(widths, plain.lines().map(str::len).collect::<Vec<_>>());
        }
    }

    #[test]
    fn column_counts_ignore_blank_zero_and_malformed_values() {
        assert_eq!(parse_column_count(" 60\t"), Some(60));
        assert_eq!(parse_column_count("0"), None);
        assert_eq!(parse_column_count("   "), None);
        assert_eq!(parse_column_count("wide"), None);
    }
}
