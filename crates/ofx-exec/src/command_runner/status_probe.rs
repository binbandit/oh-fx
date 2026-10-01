use crate::command_contract::CommandStatus;

use super::foreground_session::{EXIT_STATUS, SIGNAL_STATUS};

const MAX_STATUS_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeState {
    Scanning,
    Frame(Vec<u8>),
    Done(Option<CommandStatus>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusProbe {
    expected: Vec<u8>,
    held: Vec<u8>,
    state: ProbeState,
}

impl StatusProbe {
    pub(super) fn new(nonce: &str) -> Self {
        Self {
            expected: super::status_prefix(nonce),
            held: Vec::new(),
            state: ProbeState::Scanning,
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut emitted = Vec::with_capacity(bytes.len());
        for (index, &byte) in bytes.iter().enumerate() {
            match &mut self.state {
                ProbeState::Done(_) => {
                    emitted.extend_from_slice(&bytes[index..]);
                    break;
                }
                ProbeState::Frame(body) if byte == b'\n' => {
                    self.state = ProbeState::Done(parse_status(body));
                }
                ProbeState::Frame(body) if body.len() < MAX_STATUS_BYTES => body.push(byte),
                ProbeState::Frame(_) => {
                    self.state = ProbeState::Done(None);
                    emitted.push(byte);
                }
                ProbeState::Scanning => self.scan(byte, &mut emitted),
            }
        }
        emitted
    }

    fn scan(&mut self, byte: u8, emitted: &mut Vec<u8>) {
        if self.held.is_empty() && byte != self.expected[0] {
            emitted.push(byte);
            return;
        }
        self.held.push(byte);
        while !self.expected.starts_with(&self.held) {
            emitted.push(self.held.remove(0));
        }
        if self.held.len() == self.expected.len() {
            self.held.clear();
            self.state = ProbeState::Frame(Vec::new());
        }
    }

    pub(super) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    pub(super) fn reported(&self) -> Option<CommandStatus> {
        match self.state {
            ProbeState::Done(status) => status,
            ProbeState::Scanning | ProbeState::Frame(_) => None,
        }
    }
}

fn parse_status(body: &[u8]) -> Option<CommandStatus> {
    let body = std::str::from_utf8(body).ok()?;
    if let Some(code) = body.strip_prefix(EXIT_STATUS) {
        return code
            .parse::<u8>()
            .ok()
            .map(|code| CommandStatus::ExitCode(code.into()));
    }
    body.strip_prefix(SIGNAL_STATUS)?
        .parse::<u32>()
        .ok()
        .filter(|signal| *signal > 0)
        .map(CommandStatus::Signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn frame(nonce: &str, status: &str) -> Vec<u8> {
        let mut frame = super::super::status_prefix(nonce);
        frame.extend_from_slice(status.as_bytes());
        frame.push(b'\n');
        frame
    }

    #[test]
    fn a_frame_anywhere_in_stderr_reports_the_status_and_is_removed() {
        let mut probe = StatusProbe::new(NONCE);
        let mut stream = b"partial line".to_vec();
        stream.extend_from_slice(&frame(NONCE, "exit:3"));
        stream.extend_from_slice(b"late job output\n");
        let mut emitted = Vec::new();
        for chunk in stream.chunks(5) {
            emitted.extend_from_slice(&probe.feed(chunk));
        }
        emitted.extend_from_slice(&probe.flush());
        assert_eq!(emitted, b"partial linelate job output\n");
        assert_eq!(probe.reported(), Some(CommandStatus::ExitCode(3)));
    }

    #[test]
    fn a_signal_frame_reports_the_signal() {
        let mut probe = StatusProbe::new(NONCE);
        assert!(probe.feed(&frame(NONCE, "signal:15")).is_empty());
        assert_eq!(probe.reported(), Some(CommandStatus::Signal(15)));
    }

    #[test]
    fn partial_matches_and_frames_with_another_nonce_are_ordinary_output() {
        let mut probe = StatusProbe::new(NONCE);
        assert!(probe.feed(b"\0OH_FX").is_empty());
        assert_eq!(probe.feed(b"!\0\0OH"), b"\0OH_FX!\0");
        assert_eq!(probe.flush(), b"\0OH");
        assert_eq!(probe.reported(), None);

        let mut probe = StatusProbe::new(NONCE);
        let forged = frame("ffffffffffffffffffffffffffffffff", "exit:0");
        assert_eq!(probe.feed(&forged), forged);
        assert_eq!(probe.reported(), None);
    }

    #[test]
    fn malformed_or_unfinished_frames_report_nothing() {
        let mut probe = StatusProbe::new(NONCE);
        assert!(probe.feed(&frame(NONCE, "exit:999")).is_empty());
        assert_eq!(probe.reported(), None);
        assert_eq!(probe.feed(b"after"), b"after");

        let mut probe = StatusProbe::new(NONCE);
        let mut overlong = super::super::status_prefix(NONCE);
        overlong.extend_from_slice(&[b'9'; MAX_STATUS_BYTES]);
        assert!(probe.feed(&overlong).is_empty());
        assert_eq!(probe.feed(b"!rest"), b"!rest");
        assert_eq!(probe.reported(), None);

        let mut probe = StatusProbe::new(NONCE);
        let unfinished = frame(NONCE, "exit:0");
        assert!(probe.feed(&unfinished[..unfinished.len() - 1]).is_empty());
        assert!(probe.flush().is_empty());
        assert_eq!(probe.reported(), None);
    }
}
