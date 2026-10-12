use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};

use ofx_contract::{BoxFuture, QuestionAsker, QuestionBatchEntry, QuestionOption};

use crate::terminal_lines::{Line, next_line};

const ANSWER_WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

pub(crate) struct TerminalQuestions;

impl QuestionAsker for TerminalQuestions {
    fn ask(&self, entries: Vec<QuestionBatchEntry>) -> BoxFuture<'static, Option<Vec<String>>> {
        Box::pin(async move {
            let mut answers = Vec::with_capacity(entries.len());
            for entry in &entries {
                let mut stderr = io::stderr();
                let bell = stderr.is_terminal();
                write_prompt(entry, &mut stderr, bell).ok()?;
                let line = next_line().await.ok()?;
                answers.push(answer_line(&entry.options, &line)?);
            }
            Some(answers)
        })
    }
}

fn write_prompt(entry: &QuestionBatchEntry, output: &mut impl Write, bell: bool) -> io::Result<()> {
    output.write_all(prompt(entry).as_bytes())?;
    if bell {
        output.write_all(b"\x07")?;
    }
    output.flush()
}

fn answer_line(options: &[QuestionOption], line: &Line) -> Option<String> {
    let Line::Text(line) = line else {
        return None;
    };
    Some(answer(options, &String::from_utf8_lossy(line)))
}

fn prompt(entry: &QuestionBatchEntry) -> String {
    let mut prompt = format!("\n{}\n", entry.question);
    for (index, option) in entry.options.iter().enumerate() {
        let _ = write!(prompt, "  {}. {}", index + 1, option.label);
        if let Some(description) = &option.description {
            let _ = write!(prompt, " \u{2014} {description}");
        }
        prompt.push('\n');
    }
    prompt.push_str(if entry.options.is_empty() {
        "Enter a value: "
    } else {
        "Choose a number or enter a value: "
    });
    prompt
}

fn answer(options: &[QuestionOption], line: &str) -> String {
    let trimmed = line.trim_matches(ANSWER_WHITESPACE);
    let chosen = ordinal(trimmed)
        .and_then(|ordinal| ordinal.checked_sub(1))
        .and_then(|index| options.get(index))
        .or_else(|| options.iter().find(|option| option.label == trimmed));
    chosen.map_or(trimmed, |option| &option.label).to_owned()
}

fn ordinal(text: &str) -> Option<usize> {
    let digits = text.strip_prefix('+').unwrap_or(text);
    if digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    digits
        .bytes()
        .filter(|byte| *byte != b'_')
        .try_fold(0_usize, |value, byte| {
            let digit = char::from(byte).to_digit(10)?;
            value
                .checked_mul(10)?
                .checked_add(usize::try_from(digit).ok()?)
        })
        .filter(|_| !digits.is_empty())
}

#[cfg(test)]
mod tests;
