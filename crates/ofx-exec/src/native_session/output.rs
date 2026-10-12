use std::io;
use std::mem;
use std::os::fd::OwnedFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;

const MAX_UNREAD_HEAD_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const ESCAPE: u8 = 0x1b;
const FOLLOW_UP_POLL: Duration = Duration::from_millis(1);

#[derive(Debug, Default)]
pub(super) struct Unread {
    head: Vec<u8>,
    total: u64,
    escape: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Drained {
    pub(super) head: Vec<u8>,
    pub(super) total_bytes: u64,
    pub(super) contains_escape: bool,
    pub(super) incomplete: bool,
}

impl Unread {
    pub(super) fn append(&mut self, bytes: &[u8]) {
        let room = MAX_UNREAD_HEAD_BYTES - self.head.len();
        self.head.extend_from_slice(&bytes[..room.min(bytes.len())]);
        self.total = self
            .total
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.escape |= bytes.contains(&ESCAPE);
    }

    pub(super) fn drain(&mut self) -> Drained {
        let drained = Drained {
            head: mem::take(&mut self.head),
            total_bytes: self.total,
            contains_escape: self.escape,
            incomplete: self.total > MAX_JOURNAL_BYTES,
        };
        *self = Self::default();
        drained
    }
}

pub(super) fn read_available(
    fd: &OwnedFd,
    buffer: &mut [u8],
    timeout: Duration,
) -> io::Result<usize> {
    let mut total = 0;
    let mut wait = Timespec::try_from(timeout).unwrap_or_default();
    while total < buffer.len() {
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match poll(&mut fds, Some(&wait)) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
        let revents = fds[0].revents();
        if revents.is_empty() {
            break;
        }
        let closed = revents.intersects(PollFlags::HUP | PollFlags::ERR);
        if !revents.intersects(PollFlags::IN) && !closed {
            if total == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            break;
        }
        match rustix::io::read(fd, &mut buffer[total..]) {
            Ok(0) if total == 0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(0) => break,
            Ok(count) => total += count,
            Err(Errno::INTR) => continue,
            Err(Errno::AGAIN) if total == 0 && closed => {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Err(Errno::AGAIN) => break,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error.into()),
        }
        wait = Timespec::try_from(FOLLOW_UP_POLL).unwrap_or_default();
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn unread_output_keeps_its_head_counts_everything_and_drains_once() {
        let mut unread = Unread::default();
        unread.append(b"first ");
        unread.append(b"second\x1b[0m");
        let drained = unread.drain();
        assert_eq!(drained.head, b"first second\x1b[0m");
        assert_eq!(drained.total_bytes, 16);
        assert!(drained.contains_escape);
        assert!(!drained.incomplete);
        assert_eq!(
            unread.drain(),
            Drained {
                head: Vec::new(),
                total_bytes: 0,
                contains_escape: false,
                incomplete: false
            }
        );
        let chunk = vec![b'x'; MAX_UNREAD_HEAD_BYTES];
        for _ in 0..65 {
            unread.append(&chunk);
        }
        let drained = unread.drain();
        assert_eq!(drained.head.len(), MAX_UNREAD_HEAD_BYTES);
        assert_eq!(drained.total_bytes, 65 * 1024 * 1024);
        assert!(drained.incomplete);
        assert!(!drained.contains_escape);
    }

    #[test]
    fn terminal_output_drain_reads_final_bytes_after_peer_close() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"FINAL_OUTPUT_SENTINEL").unwrap();
        drop(writer);
        let reader = OwnedFd::from(reader);
        let mut buffer = [0; 128];
        let count = read_available(&reader, &mut buffer, Duration::from_secs(20)).unwrap();
        assert_eq!(&buffer[..count], b"FINAL_OUTPUT_SENTINEL");
        assert_eq!(
            read_available(&reader, &mut buffer, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn an_idle_terminal_reads_nothing_until_its_poll_expires() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        let reader = OwnedFd::from(reader);
        let mut buffer = [0; 16];
        assert_eq!(
            read_available(&reader, &mut buffer, Duration::from_millis(1)).unwrap(),
            0
        );
    }
}
