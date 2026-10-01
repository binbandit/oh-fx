use std::fs;
use std::path::Path;

use ra_ap_rustc_lexer::{FrontmatterAllowed, TokenKind, tokenize};

use crate::workspace_files;

type CommentScanner = fn(&str) -> Vec<usize>;

pub(crate) fn check_workspace() -> Result<(), String> {
    let mut findings = Vec::new();
    for path in workspace_files::listed_files()? {
        if !may_contain_code(Path::new(&path)) {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(scan) = comment_scanner(Path::new(&path), &source) else {
            continue;
        };
        findings.extend(
            scan(&source)
                .into_iter()
                .map(|line| format!("{path}:{line}")),
        );
    }
    crate::report(
        "comments are not allowed; make the code explain itself instead",
        &findings,
    )
}

fn may_contain_code(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        None | Some("rs" | "sh" | "toml")
    )
}

fn comment_scanner(path: &Path, source: &str) -> Option<CommentScanner> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => Some(rust_comment_lines),
        Some("sh" | "toml") => Some(hash_comment_lines),
        None if source.starts_with("#!") => Some(hash_comment_lines),
        _ => None,
    }
}

fn rust_comment_lines(source: &str) -> Vec<usize> {
    let mut remaining = source;
    let mut line = 1;
    let mut lines = Vec::new();
    for token in tokenize(source, FrontmatterAllowed::No) {
        let (text, rest) = remaining.split_at(token.len as usize);
        if matches!(
            token.kind,
            TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        ) {
            lines.push(line);
        }
        line += text.matches('\n').count();
        remaining = rest;
    }
    lines
}

fn hash_comment_lines(source: &str) -> Vec<usize> {
    source
        .lines()
        .enumerate()
        .filter(|(index, line)| {
            let trimmed = line.trim_start();
            let is_shebang = *index == 0 && trimmed.starts_with("#!");
            !is_shebang && trimmed.starts_with('#')
        })
        .map(|(index, _)| index + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_rust_line_block_and_doc_comments() {
        let source = "fn a() {}\n// note\n/* block */\n/// doc\n//! inner\nfn b() {}\n";
        assert_eq!(rust_comment_lines(source), vec![2, 3, 4, 5]);
    }

    #[test]
    fn ignores_comment_markers_inside_literals() {
        let source =
            "const URL: &str = \"https://example.com\";\nconst RAW: &str = r#\"/* x */\"#;\n";
        assert!(rust_comment_lines(source).is_empty());
    }

    #[test]
    fn allows_attributes_and_shebangs() {
        let attributes = "#![forbid(unsafe_code)]\n#[derive(Debug)]\nstruct A;\n";
        assert!(rust_comment_lines(attributes).is_empty());
        let script = "#!/bin/sh\nset -eu\n  # note\necho '#'\n";
        assert_eq!(hash_comment_lines(script), vec![3]);
    }

    #[test]
    fn scans_extensionless_files_only_with_a_shebang() {
        assert!(comment_scanner(Path::new(".githooks/pre-push"), "#!/bin/sh\n").is_some());
        assert!(comment_scanner(Path::new("LICENSE"), "# License\n").is_none());
    }
}
