mod json_document;
mod words;

use json_document::is_json_document;
use words::Words;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeywordCase {
    Sensitive,
    AsciiInsensitive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockComment {
    pub(crate) start: &'static str,
    pub(crate) end: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detection {
    None,
    TypescriptAssertion,
    Json,
    ShellShebang,
    PythonHeader,
    SqlSelect,
    DockerfileFrom,
    GoPackage,
    RustFunction,
    DiffPatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfileFlag {
    DollarVars,
    DashFlags,
    CommandWords,
    PlainBareNumbers,
    DiffLines,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Profile {
    pub label: &'static str,
    aliases: &'static Words,
    pub(crate) line_comments: &'static [&'static str],
    pub(crate) block_comment: Option<BlockComment>,
    pub(crate) quotes: &'static [u8],
    pub(crate) operators: &'static [u8],
    flags: &'static [ProfileFlag],
    pub(crate) keywords: &'static Words,
    pub(crate) literals: &'static Words,
    pub(crate) keyword_case: KeywordCase,
    detection: Detection,
}

impl Profile {
    const fn new(label: &'static str, aliases: &'static Words) -> Self {
        Self {
            label,
            aliases,
            line_comments: &[],
            block_comment: None,
            quotes: &[],
            operators: &[],
            flags: &[],
            keywords: NO_WORDS,
            literals: NO_WORDS,
            keyword_case: KeywordCase::Sensitive,
            detection: Detection::None,
        }
    }

    fn has(&self, flag: ProfileFlag) -> bool {
        self.flags.contains(&flag)
    }

    pub(crate) fn dollar_vars(&self) -> bool {
        self.has(ProfileFlag::DollarVars)
    }

    pub(crate) fn dash_flags(&self) -> bool {
        self.has(ProfileFlag::DashFlags)
    }

    pub(crate) fn command_words(&self) -> bool {
        self.has(ProfileFlag::CommandWords)
    }

    pub(crate) fn bare_numbers(&self) -> bool {
        !self.has(ProfileFlag::PlainBareNumbers)
    }

    pub fn diff_lines(&self) -> bool {
        self.has(ProfileFlag::DiffLines)
    }
}

const DOUBLE_QUOTE: &[u8] = b"\"";
const DOUBLE_SINGLE_QUOTES: &[u8] = b"\"'";
const SHELL_QUOTES: &[u8] = b"\"'`";
const SLASH_COMMENTS: &[&str] = &["//"];
const HASH_COMMENTS: &[&str] = &["#"];
const C_BLOCK_COMMENT: Option<BlockComment> = Some(BlockComment {
    start: "/*",
    end: "*/",
});
const MARKUP_BLOCK_COMMENT: Option<BlockComment> = Some(BlockComment {
    start: "<!--",
    end: "-->",
});
const NO_WORDS: &Words = &Words::new(b"");
const TRUE_FALSE_NULL: &Words = &Words::new(b"true false null");
const TRUE_FALSE_NIL: &Words = &Words::new(b"true false nil");

static PROFILES: [Profile; 40] = [
    Profile {
        line_comments: SLASH_COMMENTS,
        quotes: DOUBLE_QUOTE,
        keywords: &Words::new(
            b"const var fn pub return if else while for struct enum union try catch comptime defer \
              errdefer async await anytype void",
        ),
        ..Profile::new("zig", &Words::new(b"zig"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: SHELL_QUOTES,
        keywords: &Words::new(
            b"const let var function class interface type export import from return if else for \
              while async await new extends implements public private readonly",
        ),
        literals: &Words::new(b"true false null undefined"),
        detection: Detection::TypescriptAssertion,
        ..Profile::new("ts", &Words::new(b"js jsx javascript ts tsx typescript"))
    },
    Profile {
        quotes: DOUBLE_QUOTE,
        literals: TRUE_FALSE_NULL,
        detection: Detection::Json,
        ..Profile::new("json", &Words::new(b"json"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        operators: b"&|;<>*",
        flags: &[
            ProfileFlag::DollarVars,
            ProfileFlag::DashFlags,
            ProfileFlag::CommandWords,
            ProfileFlag::PlainBareNumbers,
        ],
        detection: Detection::ShellShebang,
        ..Profile::new("sh", &Words::new(b"sh bash zsh shell shellscript"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"def class return if elif else for while in import from as try except with lambda \
              async await pass raise yield match case",
        ),
        literals: &Words::new(b"True False None"),
        detection: Detection::PythonHeader,
        ..Profile::new("python", &Words::new(b"python py"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        literals: &Words::new(b"true false null yes no on off"),
        ..Profile::new("yaml", &Words::new(b"yaml yml"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        literals: &Words::new(b"true false"),
        ..Profile::new("toml", &Words::new(b"toml"))
    },
    Profile {
        line_comments: &["--"],
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"select from where join left right inner outer on insert into values update set \
              delete create alter drop table index group by order having limit as and or not \
              distinct union",
        ),
        literals: TRUE_FALSE_NULL,
        keyword_case: KeywordCase::AsciiInsensitive,
        detection: Detection::SqlSelect,
        ..Profile::new("sql", &Words::new(b"sql"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"from run cmd entrypoint copy add workdir env arg expose volume user label onbuild \
              stopsignal healthcheck shell maintainer",
        ),
        keyword_case: KeywordCase::AsciiInsensitive,
        detection: Detection::DockerfileFrom,
        ..Profile::new("dockerfile", &Words::new(b"dockerfile docker"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"fn let mut pub struct enum impl trait use mod crate return if else match for while \
              loop async await move where self super",
        ),
        literals: &Words::new(b"true false None Some"),
        detection: Detection::RustFunction,
        ..Profile::new("rust", &Words::new(b"rust rs"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: b"\"`",
        keywords: &Words::new(
            b"package import func var const type struct interface return if else for range switch \
              case go defer select chan map",
        ),
        literals: TRUE_FALSE_NIL,
        detection: Detection::GoPackage,
        ..Profile::new("go", &Words::new(b"go"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"auto break case char const continue default do double else enum extern float for \
              goto if int long return short signed sizeof static struct switch typedef union \
              unsigned void volatile while",
        ),
        literals: &Words::new(b"true false NULL"),
        ..Profile::new("c", &Words::new(b"c h m mm"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"auto bool class const constexpr decltype delete enum explicit friend inline \
              namespace new nullptr private protected public template this typename using virtual \
              void",
        ),
        literals: &Words::new(b"true false nullptr NULL"),
        ..Profile::new("cpp", &Words::new(b"cpp c++ cc cxx hpp"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"class namespace using public private protected internal static void string int var \
              new return if else for foreach while async await interface record get set",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("csharp", &Words::new(b"csharp cs"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"class interface package import public private protected static final void new return \
              if else for while try catch throws extends implements record var",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("java", &Words::new(b"java"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"fun val var class object interface package import public private return if else when \
              for while try catch data sealed suspend",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("kotlin", &Words::new(b"kotlin kt kts"))
    },
    Profile {
        line_comments: &["//", "#"],
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"function class public private protected namespace use return if else foreach for \
              while try catch new static const echo yield",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("php", &Words::new(b"php"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"def class module end return if elsif else unless case when do while for in begin \
              rescue require attr_reader",
        ),
        literals: TRUE_FALSE_NIL,
        ..Profile::new("ruby", &Words::new(b"ruby rb"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"func let var class struct enum protocol extension import public private return if \
              else guard for while switch case async await throws try",
        ),
        literals: TRUE_FALSE_NIL,
        ..Profile::new("swift", &Words::new(b"swift"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        block_comment: Some(BlockComment {
            start: "<#",
            end: "#>",
        }),
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"function param if else elseif foreach for while switch return throw try catch \
              finally begin process end filter class enum",
        ),
        literals: TRUE_FALSE_NULL,
        keyword_case: KeywordCase::AsciiInsensitive,
        ..Profile::new("powershell", &Words::new(b"powershell ps1 pwsh ps"))
    },
    Profile {
        line_comments: &["--"],
        block_comment: Some(BlockComment {
            start: "--[[",
            end: "]]",
        }),
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"and break do else elseif end false for function goto if in local nil not or repeat \
              return then true until while",
        ),
        literals: TRUE_FALSE_NIL,
        ..Profile::new("lua", &Words::new(b"lua"))
    },
    Profile {
        block_comment: MARKUP_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"html head body main header footer section article div span a p script style link \
              meta title button input form img ul li",
        ),
        ..Profile::new("html", &Words::new(b"html htm vue svelte"))
    },
    Profile {
        block_comment: MARKUP_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(b"xml version encoding DOCTYPE CDATA"),
        ..Profile::new("xml", &Words::new(b"xml"))
    },
    Profile {
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"color background display position margin padding border font width height flex grid \
              align justify transition transform animation media",
        ),
        ..Profile::new("css", &Words::new(b"css"))
    },
    Profile {
        line_comments: &["#", "//"],
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"resource module variable output provider terraform locals data dynamic for_each \
              count",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("hcl", &Words::new(b"hcl terraform tf"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        flags: &[ProfileFlag::DollarVars],
        ..Profile::new("make", &Words::new(b"make makefile mk"))
    },
    Profile {
        line_comments: &["#", ";"],
        ..Profile::new("ini", &Words::new(b"ini conf cfg editorconfig"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        ..Profile::new("dotenv", &Words::new(b"dotenv env"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_QUOTE,
        keywords: &Words::new(
            b"query mutation subscription fragment on type input interface enum union scalar \
              schema extend implements directive",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("graphql", &Words::new(b"graphql gql"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"const final var class extends with implements mixin enum if else for while return \
              async await new static import export void",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("dart", &Words::new(b"dart"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_QUOTE,
        keywords: &Words::new(
            b"val var def class object trait extends with package import if else for while yield \
              match case return new type given override",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("scala", &Words::new(b"scala sc"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_QUOTE,
        keywords: &Words::new(
            b"def defmodule defp defmacro defguard do end fn if else unless case cond when with \
              for try rescue after alias import require use",
        ),
        literals: TRUE_FALSE_NIL,
        ..Profile::new("elixir", &Words::new(b"elixir ex exs"))
    },
    Profile {
        line_comments: &["--"],
        block_comment: Some(BlockComment {
            start: "{-",
            end: "-}",
        }),
        quotes: DOUBLE_QUOTE,
        keywords: &Words::new(
            b"module where import data type newtype class instance deriving if then else case of \
              do let in infix infixl infixr",
        ),
        literals: &Words::new(b"True False"),
        ..Profile::new("haskell", &Words::new(b"haskell hs"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: SHELL_QUOTES,
        flags: &[ProfileFlag::DollarVars],
        keywords: &Words::new(
            b"my our sub use package if else elsif unless while for foreach return local state say \
              print die warn eval do require",
        ),
        literals: &Words::new(b"undef"),
        ..Profile::new("perl", &Words::new(b"perl pl pm"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"function if else for while repeat break next return in library require",
        ),
        literals: &Words::new(b"TRUE FALSE NULL NA"),
        ..Profile::new("r", &Words::new(b"r"))
    },
    Profile {
        line_comments: SLASH_COMMENTS,
        block_comment: C_BLOCK_COMMENT,
        quotes: DOUBLE_SINGLE_QUOTES,
        keywords: &Words::new(
            b"def class interface enum if else for while return new try catch finally throw \
              package import extends implements static final void",
        ),
        literals: TRUE_FALSE_NULL,
        ..Profile::new("groovy", &Words::new(b"groovy gradle"))
    },
    Profile {
        line_comments: HASH_COMMENTS,
        keywords: &Words::new(
            b"server location listen root proxy_pass set return rewrite if error_page access_log \
              include upstream worker_processes events http",
        ),
        ..Profile::new("nginx", &Words::new(b"nginx"))
    },
    Profile {
        block_comment: MARKUP_BLOCK_COMMENT,
        quotes: b"`",
        flags: &[ProfileFlag::PlainBareNumbers],
        ..Profile::new("markdown", &Words::new(b"md markdown mdx"))
    },
    Profile {
        flags: &[ProfileFlag::PlainBareNumbers],
        ..Profile::new("text", &Words::new(b"text txt plain plaintext"))
    },
    Profile {
        flags: &[ProfileFlag::DiffLines],
        detection: Detection::DiffPatch,
        ..Profile::new("diff", &Words::new(b"diff patch"))
    },
];

pub fn resolve(label: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| {
        profile
            .aliases
            .contains(label, KeywordCase::AsciiInsensitive)
    })
}

pub fn infer(source: &str) -> Option<&'static Profile> {
    PROFILES
        .iter()
        .find(|profile| matches_detection(profile.detection, source))
}

fn matches_detection(detection: Detection, source: &str) -> bool {
    match detection {
        Detection::None => false,
        Detection::TypescriptAssertion => matches_typescript_assertion(source),
        Detection::Json => is_valid_json(source),
        Detection::ShellShebang => matches_shell_shebang(source),
        Detection::PythonHeader => matches_python_header(source),
        Detection::SqlSelect => matches_sql_select(source),
        Detection::DockerfileFrom => starts_with_ignore_case(first_nonblank_line(source), "from "),
        Detection::GoPackage => {
            first_nonblank_line(source).starts_with("package ")
                && contains_line_start(source, "func ")
        }
        Detection::RustFunction => matches_rust_function(source),
        Detection::DiffPatch => matches_diff_patch(source),
    }
}

fn matches_diff_patch(source: &str) -> bool {
    let line = first_nonblank_line(source);
    if line.starts_with("diff --git ") || line.starts_with("@@ ") {
        return true;
    }
    line.starts_with("--- ") && source.contains("\n+++ ")
}

fn matches_typescript_assertion(source: &str) -> bool {
    source.match_indices("} as ").any(|(start, needle)| {
        source
            .as_bytes()
            .get(start + needle.len())
            .is_some_and(u8::is_ascii_uppercase)
    })
}

fn is_valid_json(source: &str) -> bool {
    let trimmed = source.trim_matches([' ', '\t', '\r', '\n']);
    if !trimmed.starts_with(['{', '[']) {
        return false;
    }
    is_json_document(trimmed)
}

fn matches_shell_shebang(source: &str) -> bool {
    let line = first_nonblank_line(source);
    line.starts_with("#!")
        && (line.contains("bash") || line.contains("zsh") || line.contains("/sh"))
}

fn matches_python_header(source: &str) -> bool {
    let line = first_nonblank_line(source);
    (line.starts_with("def ") || line.starts_with("class ")) && line.ends_with(':')
}

fn matches_sql_select(source: &str) -> bool {
    starts_with_ignore_case(first_nonblank_line(source), "select ")
        && contains_word_ignore_case(source, "from")
}

fn matches_rust_function(source: &str) -> bool {
    let line = first_nonblank_line(source);
    if !line.starts_with("fn ") && !line.starts_with("pub fn ") {
        return false;
    }
    source.contains("let ") || source.contains("println!") || line.contains("->")
}

fn first_nonblank_line(source: &str) -> &str {
    source
        .split('\n')
        .map(|line| line.trim_matches([' ', '\t', '\r']))
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn contains_line_start(source: &str, prefix: &str) -> bool {
    source.starts_with(prefix)
        || source
            .match_indices('\n')
            .any(|(newline, _)| source[newline + 1..].starts_with(prefix))
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn contains_word_ignore_case(source: &str, word: &str) -> bool {
    let bytes = source.as_bytes();
    let word = word.as_bytes();
    if word.len() > bytes.len() {
        return false;
    }
    (0..=bytes.len() - word.len()).any(|index| {
        let end = index + word.len();
        bytes[index..end].eq_ignore_ascii_case(word)
            && (index == 0 || !is_word_byte(bytes[index - 1]))
            && (end == bytes.len() || !is_word_byte(bytes[end]))
    })
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_script_assertions_infer_the_canonical_type_script_label() {
        let source = "const hook = await resumeHook(token, { cleanup: true } as CleanupSignal);";
        assert_eq!(infer(source).map(|profile| profile.label), Some("ts"));
        assert!(infer("const value = 1;").is_none());
        assert!(infer("const value = {} as cleanupSignal;").is_none());
    }

    #[test]
    fn supported_code_fence_labels_resolve_case_insensitively() {
        let cases = [
            ("Zig", "zig"),
            ("js", "ts"),
            ("JSX", "ts"),
            ("javascript", "ts"),
            ("TS", "ts"),
            ("tsx", "ts"),
            ("TypeScript", "ts"),
            ("JSON", "json"),
            ("sh", "sh"),
            ("BASH", "sh"),
            ("zsh", "sh"),
            ("Shell", "sh"),
        ];
        for (label, profile) in cases {
            assert_eq!(resolve(label).map(|found| found.label), Some(profile));
        }
        assert!(resolve("").is_none());
        assert_eq!(resolve("text").map(|found| found.label), Some("text"));
    }

    #[test]
    fn expanded_code_fence_labels_resolve_through_the_language_registry() {
        let cases = [
            ("python", "python"),
            ("py", "python"),
            ("yaml", "yaml"),
            ("yml", "yaml"),
            ("toml", "toml"),
            ("sql", "sql"),
            ("dockerfile", "dockerfile"),
            ("rust", "rust"),
            ("rs", "rust"),
            ("go", "go"),
            ("c", "c"),
            ("cpp", "cpp"),
            ("c++", "cpp"),
            ("csharp", "csharp"),
            ("cs", "csharp"),
            ("java", "java"),
            ("kotlin", "kotlin"),
            ("php", "php"),
            ("ruby", "ruby"),
            ("swift", "swift"),
            ("powershell", "powershell"),
            ("ps1", "powershell"),
            ("lua", "lua"),
            ("html", "html"),
            ("xml", "xml"),
            ("css", "css"),
            ("hcl", "hcl"),
            ("terraform", "hcl"),
            ("tf", "hcl"),
        ];
        for (label, profile) in cases {
            assert_eq!(resolve(label).map(|found| found.label), Some(profile));
        }
    }

    #[test]
    fn high_confidence_source_shapes_infer_registered_profiles() {
        let cases = [
            ("{\"ready\": true}", "json"),
            ("#!/usr/bin/env bash\necho ready", "sh"),
            ("def render(value):\n    return value", "python"),
            ("SELECT id FROM users", "sql"),
            ("FROM alpine:3.20\nRUN echo ready", "dockerfile"),
            ("package main\nfunc main() {}", "go"),
            ("fn main() { println!(\"ready\"); }", "rust"),
        ];
        for (source, profile) in cases {
            assert_eq!(infer(source).map(|found| found.label), Some(profile));
        }
        assert!(infer("const value = 1;").is_none());
        assert!(infer("title: ready").is_none());
    }

    #[test]
    fn json_inference_accepts_what_zig_std_json_accepts() {
        let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        for source in [
            "[1e400]".to_owned(),
            "[-1e400, 123456789012345678901234567890, 1.0e-400, -0]".to_owned(),
            nested(128),
            nested(100_000),
            " \t{\"a\": {\"a\": [true, false, null, \"\\ud83d\\ude00\\n\"]}}\r\n".to_owned(),
        ] {
            assert_eq!(infer(&source).map(|found| found.label), Some("json"));
        }
        for source in [
            "{\"a\":1,\"a\":2}",
            "{\"a\":1,\"\\u0061\":2}",
            "[\"\\ud800\"]",
            "[1,]",
            "[01]",
            "{\"a\":\"\u{1}\"}",
            "\u{feff}[1]",
        ] {
            assert!(infer(source).is_none(), "{source:?}");
        }
    }

    #[test]
    fn aliases_do_not_collide_across_profiles() {
        for (index, profile) in PROFILES.iter().enumerate() {
            for alias in profile.aliases.iter() {
                for other in &PROFILES[index + 1..] {
                    assert!(
                        other
                            .aliases
                            .iter()
                            .all(|other_alias| !alias.eq_ignore_ascii_case(other_alias))
                    );
                }
            }
        }
    }

    #[test]
    fn resolve_covers_the_added_languages_and_aliases() {
        let cases = [
            ("makefile", "make"),
            ("conf", "ini"),
            ("env", "dotenv"),
            ("gql", "graphql"),
            ("dart", "dart"),
            ("sc", "scala"),
            ("exs", "elixir"),
            ("hs", "haskell"),
            ("pl", "perl"),
            ("r", "r"),
            ("gradle", "groovy"),
            ("nginx", "nginx"),
            ("md", "markdown"),
            ("txt", "text"),
            ("patch", "diff"),
            ("shellscript", "sh"),
            ("mm", "c"),
            ("vue", "html"),
        ];
        for (alias, label) in cases {
            assert_eq!(resolve(alias).map(|profile| profile.label), Some(label));
        }
    }

    #[test]
    fn infer_detects_diff_patches_without_a_fence_label() {
        assert_eq!(
            infer("--- a/main.zig\n+++ b/main.zig\n@@ -1 +1 @@\n-old\n+new").map(|p| p.label),
            Some("diff")
        );
        assert!(infer("plain prose about --- things").is_none());
    }
}
