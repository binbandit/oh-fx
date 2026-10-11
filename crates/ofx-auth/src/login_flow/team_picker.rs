use std::fmt::Write as _;
use std::io::Write;
use std::os::fd::AsFd;

use ofx_text::encode_terminal_safe;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::{Team, VercelError, write_all};
use crate::raw_input::RawInput;

const HEADING: &str = "\nSelect a Vercel team for AI Gateway:\nModel requests will use the selected team; Gateway access may still require billing or API setup.\n\n";
const INVALID_CHOICE: &str = "Enter a team number from the list.\n";
const MAX_LINE_BYTES: usize = 1024;
const MAX_KEY_BYTES: usize = 8;
const ESCAPE: u8 = 0x1b;
const ESCAPE_WAIT_NS: i64 = 25_000_000;
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Number(usize),
    Ignored,
}

pub(super) fn select_by_line<F: AsFd>(
    teams: &[Team],
    default_index: usize,
    input: &F,
    output: &mut dyn Write,
) -> Result<usize, VercelError> {
    write_all(output, HEADING)?;
    for (index, team) in teams.iter().enumerate() {
        let marker = if index == default_index {
            " (default)"
        } else {
            ""
        };
        write_all(
            output,
            &format!("  {}. {}{marker}\n", index + 1, team_label(team)),
        )?;
    }
    loop {
        write_all(output, &format!("Team [{}]: ", default_index + 1))?;
        let line = read_line(input);
        let choice = line.trim_matches(TRIMMED);
        if choice.is_empty() {
            return Ok(default_index);
        }
        match parse_choice(choice) {
            Some(number) if (1..=teams.len()).contains(&number) => return Ok(number - 1),
            _ => write_all(output, INVALID_CHOICE)?,
        }
    }
}

pub(super) fn select_interactive<F: AsFd>(
    teams: &[Team],
    default_index: usize,
    input: &F,
    output: &mut dyn Write,
) -> Result<usize, VercelError> {
    let _raw = RawInput::enable(input.as_fd()).map_err(|_| VercelError::ReadFailed)?;
    write_all(output, HEADING)?;
    let mut selected = default_index;
    render(teams, selected, default_index, true, output)?;
    loop {
        match read_key(input)? {
            Key::Up => selected = selected.checked_sub(1).unwrap_or(teams.len() - 1),
            Key::Down => selected = (selected + 1) % teams.len(),
            Key::Enter => return Ok(selected),
            Key::Cancel => return Err(VercelError::InvalidTeamSelection),
            Key::Number(number) if (1..=teams.len()).contains(&number) => return Ok(number - 1),
            Key::Number(_) | Key::Ignored => continue,
        }
        render(teams, selected, default_index, false, output)?;
    }
}

fn render(
    teams: &[Team],
    selected: usize,
    default_index: usize,
    first: bool,
    output: &mut dyn Write,
) -> Result<(), VercelError> {
    let mut frame = String::new();
    if !first {
        let _ = write!(frame, "\x1b[{}A\r", teams.len());
    }
    for (index, team) in teams.iter().enumerate() {
        let prefix = if index == selected {
            "  \u{203a} "
        } else {
            "    "
        };
        let marker = if index == default_index {
            " (current)"
        } else {
            ""
        };
        let _ = writeln!(
            frame,
            "\x1b[2K{prefix}{}. {}{marker}",
            index + 1,
            team_label(team)
        );
    }
    write_all(output, &frame)
}

fn team_label(team: &Team) -> String {
    format!(
        "{} ({})",
        encode_terminal_safe(team.name.as_bytes(), usize::MAX).text,
        encode_terminal_safe(team.slug.as_bytes(), usize::MAX).text
    )
}

fn parse_choice(choice: &str) -> Option<usize> {
    let separated = choice.starts_with('_') || choice.ends_with('_');
    let digits: String = choice
        .chars()
        .filter(|character| *character != '_')
        .collect();
    (!separated && !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn read_line<F: AsFd>(input: &F) -> String {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match rustix::io::read(input, &mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() < MAX_LINE_BYTES => line.push(byte[0]),
            Ok(_) | Err(_) => return String::new(),
        }
    }
    String::from_utf8_lossy(&line).into_owned()
}

fn read_key<F: AsFd>(input: &F) -> Result<Key, VercelError> {
    let mut bytes = [0_u8; MAX_KEY_BYTES];
    let first = rustix::io::read(input, &mut bytes[..1]).map_err(|_| VercelError::ReadFailed)?;
    if first == 0 {
        return Ok(Key::Ignored);
    }
    let mut len = first;
    if bytes[0] == ESCAPE {
        while len < MAX_KEY_BYTES && !escape_sequence_complete(&bytes[..len]) {
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: ESCAPE_WAIT_NS,
            };
            let mut descriptors = [PollFd::new(input, PollFlags::IN)];
            let ready =
                poll(&mut descriptors, Some(&timeout)).map_err(|_| VercelError::ReadFailed)?;
            if ready == 0 || !descriptors[0].revents().contains(PollFlags::IN) {
                break;
            }
            let read = rustix::io::read(input, &mut bytes[len..=len])
                .map_err(|_| VercelError::ReadFailed)?;
            if read == 0 {
                break;
            }
            len += read;
        }
    }
    Ok(parse_key(&bytes[..len]))
}

fn escape_sequence_complete(bytes: &[u8]) -> bool {
    if bytes.len() < 3 || bytes[0] != ESCAPE {
        return false;
    }
    if bytes[1] != b'[' && bytes[1] != b'O' {
        return true;
    }
    let last = bytes[bytes.len() - 1];
    last.is_ascii_uppercase() || last == b'~'
}

fn parse_key(bytes: &[u8]) -> Key {
    match bytes.first() {
        Some(b'\r' | b'\n') => Key::Enter,
        Some(3 | 4) => Key::Cancel,
        Some(digit @ b'1'..=b'9') => Key::Number(usize::from(digit - b'0')),
        Some(&ESCAPE) if bytes.len() >= 3 && matches!(bytes[1], b'[' | b'O') => match bytes[2] {
            b'A' => Key::Up,
            b'B' => Key::Down,
            _ => Key::Ignored,
        },
        Some(&ESCAPE) => Key::Cancel,
        _ => Key::Ignored,
    }
}

#[cfg(test)]
mod tests;
