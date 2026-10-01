mod budgets;
mod build;
mod measure;
mod metric;
mod report;
mod repository;
mod scenario;

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const BUDGETS: &str = "budgets.toml";
const DEFAULT_BASE_BRANCH: &str = "origin/main";
const USAGE: &str =
    "usage: cargo xtask footprint [--base <commit>] [--summary <file>] [--pr-body <file>]";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    base: Option<String>,
    summary: Option<PathBuf>,
    pr_body: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[&str]) -> Result<(), String> {
    let invoked_from =
        env::current_dir().map_err(|error| format!("read the current directory: {error}"))?;
    let options = resolve(parse_options(args)?, &invoked_from);
    repository::enter_root()?;
    let budgets = budgets::load(Path::new(BUDGETS))?;
    let head = revision("HEAD")?;
    let base = match &options.base {
        Some(base) => revision(base)?,
        None => default_base(&head)?,
    };
    let trailer = match &options.pr_body {
        Some(path) => {
            report::trailer_reason(&repository::read(path)?, &budgets.pull_request.trailer)
        }
        None => None,
    };
    let builds = build::Builds::prepare(&invoked_from)?;
    let head_release = builds.head()?;
    let head_readings = measure::measure(&head_release);
    let base_readings = if base == head {
        Ok(head_readings.clone())
    } else {
        builds.base(&base).map(|release| measure::measure(&release))
    };
    let text = report::render(
        &budgets,
        &report::Side {
            commit: &head,
            readings: Ok(&head_readings),
        },
        &report::Side {
            commit: &base,
            readings: base_readings.as_ref().map_err(String::as_str),
        },
        build::TARGET,
        trailer.as_deref(),
    );
    print!("{text}");
    if let Some(path) = &options.summary {
        append(path, &text)?;
    }
    Ok(())
}

fn parse_options(args: &[&str]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut remaining = args.iter();
    while let Some(flag) = remaining.next() {
        let value = remaining.next().ok_or_else(|| USAGE.to_owned())?;
        match *flag {
            "--base" => options.base = Some((*value).to_owned()),
            "--summary" => options.summary = Some(PathBuf::from(value)),
            "--pr-body" => options.pr_body = Some(PathBuf::from(value)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(options)
}

fn resolve(options: Options, invoked_from: &Path) -> Options {
    Options {
        summary: options.summary.map(|path| invoked_from.join(path)),
        pr_body: options.pr_body.map(|path| invoked_from.join(path)),
        ..options
    }
}

fn revision(name: &str) -> Result<String, String> {
    Ok(
        repository::git(&["rev-parse", "--verify", &format!("{name}^{{commit}}")])?
            .trim()
            .to_owned(),
    )
}

fn default_base(head: &str) -> Result<String, String> {
    let merge_base = repository::git(&["merge-base", "HEAD", DEFAULT_BASE_BRANCH])?
        .trim()
        .to_owned();
    if merge_base == head {
        revision("HEAD^")
    } else {
        Ok(merge_base)
    }
}

fn append(path: &Path, text: &str) -> Result<(), String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_in_any_order() {
        let options = parse_options(&["--summary", "out.md", "--base", "abc", "--pr-body", "body"])
            .expect("valid flags");
        assert_eq!(
            options,
            Options {
                base: Some("abc".to_owned()),
                summary: Some(PathBuf::from("out.md")),
                pr_body: Some(PathBuf::from("body")),
            }
        );
        assert_eq!(parse_options(&[]), Ok(Options::default()));
    }

    #[test]
    fn rejects_unknown_or_incomplete_flags() {
        assert_eq!(parse_options(&["--base"]), Err(USAGE.to_owned()));
        assert_eq!(parse_options(&["--enforce", "yes"]), Err(USAGE.to_owned()));
    }

    #[test]
    fn resolves_output_paths_against_the_invoking_directory() {
        let options = resolve(
            Options {
                base: None,
                summary: Some(PathBuf::from("report.md")),
                pr_body: Some(PathBuf::from("/tmp/body.txt")),
            },
            Path::new("/work/oh-fx/crates"),
        );
        assert_eq!(
            options.summary,
            Some(PathBuf::from("/work/oh-fx/crates/report.md"))
        );
        assert_eq!(options.pr_body, Some(PathBuf::from("/tmp/body.txt")));
    }
}
