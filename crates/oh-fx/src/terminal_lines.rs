use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

use tokio::sync::oneshot;

pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line {
    Text(Vec<u8>),
    TooLong,
    Closed,
}

#[derive(Default)]
struct Lines {
    started: bool,
    closed: bool,
    ready: VecDeque<Line>,
    waiters: VecDeque<oneshot::Sender<Line>>,
}

static LINES: OnceLock<Mutex<Lines>> = OnceLock::new();

pub(crate) fn next_line() -> oneshot::Receiver<Line> {
    let (sender, receiver) = oneshot::channel();
    let mut lines = lock();
    if let Some(line) = lines.ready.pop_front() {
        let _ = sender.send(line);
        return receiver;
    }
    if lines.closed {
        let _ = sender.send(Line::Closed);
        return receiver;
    }
    lines.waiters.push_back(sender);
    if !std::mem::replace(&mut lines.started, true)
        && thread::Builder::new()
            .name("terminal-lines".to_owned())
            .spawn(read_stdin)
            .is_err()
    {
        close(&mut lines);
    }
    receiver
}

fn read_stdin() {
    let mut stdin = io::stdin().lock();
    loop {
        let line = read_line(&mut stdin);
        let closed = line == Line::Closed;
        deliver(line);
        if closed {
            return;
        }
    }
}

fn deliver(line: Line) {
    let mut lines = lock();
    if line == Line::Closed {
        close(&mut lines);
        return;
    }
    let mut line = line;
    while let Some(waiter) = lines.waiters.pop_front() {
        match waiter.send(line) {
            Ok(()) => return,
            Err(undelivered) => line = undelivered,
        }
    }
    lines.ready.push_back(line);
}

fn close(lines: &mut Lines) {
    lines.closed = true;
    for waiter in lines.waiters.drain(..) {
        let _ = waiter.send(Line::Closed);
    }
}

pub(crate) fn read_line(input: &mut impl Read) -> Line {
    let mut line = Vec::new();
    let mut too_long = false;
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) if line.is_empty() && !too_long => return Line::Closed,
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() == MAX_LINE_BYTES => too_long = true,
            Ok(_) => line.push(byte[0]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Line::Closed,
        }
    }
    if too_long {
        Line::TooLong
    } else {
        Line::Text(line)
    }
}

fn lock() -> MutexGuard<'static, Lines> {
    LINES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_read_one_at_a_time_and_capped() {
        let input = format!("first\nsecond\n{}\nlast", "x".repeat(MAX_LINE_BYTES + 1));
        let mut input = input.as_bytes();
        assert_eq!(read_line(&mut input), Line::Text(b"first".to_vec()));
        assert_eq!(read_line(&mut input), Line::Text(b"second".to_vec()));
        assert_eq!(read_line(&mut input), Line::TooLong);
        assert_eq!(read_line(&mut input), Line::Text(b"last".to_vec()));
        assert_eq!(read_line(&mut input), Line::Closed);
    }

    #[tokio::test]
    async fn a_line_skips_waiters_that_stopped_waiting() {
        let mut lines = Lines::default();
        let (abandoned, gone) = oneshot::channel();
        let (waiting, answer) = oneshot::channel();
        lines.waiters.extend([abandoned, waiting]);
        drop(gone);
        *lock() = lines;
        deliver(Line::Text(b"y".to_vec()));
        assert_eq!(answer.await.unwrap(), Line::Text(b"y".to_vec()));
        deliver(Line::Text(b"early".to_vec()));
        assert_eq!(
            lock().ready.pop_front(),
            Some(Line::Text(b"early".to_vec()))
        );
        deliver(Line::Closed);
        assert!(lock().closed);
    }
}
