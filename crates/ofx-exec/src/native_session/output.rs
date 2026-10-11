use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;

const FOLLOW_UP_POLL: Duration = Duration::from_millis(1);

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
