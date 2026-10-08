use std::io::{self, IsTerminal, Read, Write};
use std::thread;

use ofx_agent::Approvals;
use ofx_contract::{ApprovalDecision, ApprovalRequest, CommandRequest};
use ofx_shell::{command_risk_note, command_safer_alternative};
use ofx_text::encode_terminal_safe;

const MAX_COMMAND_LABEL_BYTES: usize = 120;
const ANSWER_BUFFER_BYTES: usize = 256;
const FILE_MUTATION_LABEL: &str = "file_mutation";
const RISK_NOTE_PREFIX: &str = "note: ";
const ANSWER_WHITESPACE: &[u8] = b" \t\r\n";

pub(crate) fn ask(approvals: &Approvals, request: &ApprovalRequest) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    write!(
        stderr,
        "oh-fx wants to run:\n  {}\n\nApprove? [y/N] ",
        permission_label(request)
    )?;
    stderr.flush()?;
    if stderr.is_terminal() {
        let _ = stderr.write_all(b"\x07").and_then(|()| stderr.flush());
    }
    drop(stderr);
    let id = request.id;
    let answering = approvals.clone();
    if thread::Builder::new()
        .name("permission-prompt".to_owned())
        .spawn(move || answering.resolve(id, decision(&mut io::stdin().lock())))
        .is_err()
    {
        approvals.resolve(id, ApprovalDecision::Deny);
    }
    Ok(())
}

fn permission_label(request: &ApprovalRequest) -> String {
    if request.file.is_some() {
        return FILE_MUTATION_LABEL.to_owned();
    }
    let label = match &request.command {
        Some(CommandRequest::Run { command, .. }) => return run_command_label(command),
        Some(CommandRequest::Observe | CommandRequest::SendInput { .. }) => {
            format!("{} interact", request.tool_name)
        }
        Some(CommandRequest::Stop) => format!("{} stop", request.tool_name),
        None => match &request.description.label {
            Some(label) => format!("{} {}", request.tool_name, label.target),
            None => request.tool_name.clone(),
        },
    };
    encode_terminal_safe(label.as_bytes(), usize::MAX).text
}

fn run_command_label(command: &str) -> String {
    let encoded = encode_terminal_safe(command.as_bytes(), MAX_COMMAND_LABEL_BYTES).text;
    let risk =
        command_risk_note(command).map(|note| note.strip_prefix(RISK_NOTE_PREFIX).unwrap_or(note));
    let suffix = match (risk, command_safer_alternative(command)) {
        (Some(risk), Some(safer)) => format!(" (risk: {risk}; {safer})"),
        (Some(risk), None) => format!(" (risk: {risk})"),
        (None, Some(safer)) => format!(" ({safer})"),
        (None, None) => String::new(),
    };
    format!("shell.run {encoded}{suffix}")
}

fn decision(input: &mut impl Read) -> ApprovalDecision {
    let mut line = Vec::new();
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() + 1 == ANSWER_BUFFER_BYTES => {
                discard_rest_of_line(input);
                return ApprovalDecision::Deny;
            }
            Ok(_) => line.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return ApprovalDecision::Deny,
        }
    }
    let start = line
        .iter()
        .position(|byte| !ANSWER_WHITESPACE.contains(byte))
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !ANSWER_WHITESPACE.contains(byte))
        .map_or(start, |last| last + 1);
    match &line[start..end] {
        b"y" | b"Y" => ApprovalDecision::Once,
        _ => ApprovalDecision::Deny,
    }
}

fn discard_rest_of_line(input: &mut impl Read) {
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(1..) if byte[0] != b'\n' => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests;
