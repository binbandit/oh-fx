mod attribution;
mod comments;
mod commit_message;
mod conventional;
mod pipeline;
mod workspace_files;

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <style | lint | test | ci | attribution <file> | commit-msg <file> | subjects <file> | title <file> | hooks>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = match args.as_slice() {
        ["style"] => pipeline::style(),
        ["lint"] => pipeline::lint(),
        ["test"] => pipeline::test(),
        ["ci"] => pipeline::lint().and_then(|()| pipeline::test()),
        ["attribution", path] => attribution::check_file(Path::new(path)),
        ["commit-msg", path] => commit_message::check(Path::new(path)),
        ["subjects", path] => conventional::check_subjects_file(Path::new(path)),
        ["title", path] => conventional::check_title_file(Path::new(path)),
        ["hooks"] => pipeline::install_hooks(),
        _ => Err(USAGE.to_owned()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn report(header: &str, findings: &[String]) -> Result<(), String> {
    if findings.is_empty() {
        Ok(())
    } else {
        Err(format!("{header}:\n{}", findings.join("\n")))
    }
}
