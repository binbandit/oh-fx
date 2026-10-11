use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::control::{CONTROL_MODE, MARKER_ACK, MARKER_FRAME_LEN, MarkerKind, NONCE_LEN};

const MARKER_ACK_TIMEOUT: Duration = Duration::from_secs(15);

pub fn is_terminal_control_invocation(args: &[OsString]) -> bool {
    matches!(args, [mode, _, _, _] if mode == CONTROL_MODE)
}

pub fn run_terminal_control(args: &[OsString]) -> ! {
    std::process::exit(i32::from(send_marker(args).is_err()))
}

fn send_marker(args: &[OsString]) -> io::Result<()> {
    let [_, control_path, nonce, event] = args else {
        return Err(invalid_marker());
    };
    let nonce = nonce.as_encoded_bytes();
    if nonce.len() != NONCE_LEN {
        return Err(invalid_marker());
    }
    let kind = event
        .to_str()
        .and_then(MarkerKind::from_event)
        .ok_or_else(invalid_marker)?;
    let mut frame = [0; MARKER_FRAME_LEN];
    frame[..NONCE_LEN].copy_from_slice(nonce);
    frame[NONCE_LEN] = kind as u8;
    let mut stream = UnixStream::connect(control_path)?;
    stream.set_read_timeout(Some(MARKER_ACK_TIMEOUT))?;
    stream.write_all(&frame)?;
    let mut ack = [0; 1];
    stream.read_exact(&mut ack)?;
    if ack[0] != MARKER_ACK {
        return Err(io::Error::other("ControlMarkerRejected"));
    }
    Ok(())
}

fn invalid_marker() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "InvalidControlMarker")
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::thread;

    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn args(path: &Path, nonce: &str, event: &str) -> Vec<OsString> {
        vec![CONTROL_MODE.into(), path.into(), nonce.into(), event.into()]
    }

    fn serve_once(listener: UnixListener, reply: Option<u8>) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = vec![0; MARKER_FRAME_LEN];
            stream.read_exact(&mut frame).unwrap();
            if let Some(reply) = reply {
                stream.write_all(&[reply]).unwrap();
            }
            frame
        })
    }

    #[test]
    fn only_the_four_argument_control_form_is_a_control_invocation() {
        let path = Path::new("/tmp/control.sock");
        assert!(is_terminal_control_invocation(&args(
            path,
            NONCE,
            "shell-ready"
        )));
        assert!(!is_terminal_control_invocation(
            &args(path, NONCE, "shell-ready")[..3]
        ));
        let mut extra = args(path, NONCE, "shell-ready");
        extra.push("more".into());
        assert!(!is_terminal_control_invocation(&extra));
        let mut other = args(path, NONCE, "shell-ready");
        other[0] = "--fx-internal-terminal-control".into();
        assert!(!is_terminal_control_invocation(&other));
    }

    #[test]
    fn a_marker_sends_its_nonce_and_kind_and_needs_the_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        for (event, kind) in [
            ("shell-ready", MarkerKind::ShellReady),
            ("command-started", MarkerKind::CommandStarted),
        ] {
            let server = serve_once(UnixListener::bind(&path).unwrap(), Some(MARKER_ACK));
            send_marker(&args(&path, NONCE, event)).unwrap();
            let mut expected = NONCE.as_bytes().to_vec();
            expected.push(kind as u8);
            assert_eq!(server.join().unwrap(), expected);
            std::fs::remove_file(&path).unwrap();
        }
        for reply in [Some(0), None] {
            let server = serve_once(UnixListener::bind(&path).unwrap(), reply);
            assert!(send_marker(&args(&path, NONCE, "shell-ready")).is_err());
            server.join().unwrap();
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn malformed_markers_fail_before_connecting() {
        let missing = Path::new("/nonexistent/control.sock");
        for marker in [
            args(missing, &NONCE[1..], "shell-ready"),
            args(missing, NONCE, "command-exited"),
            args(missing, NONCE, "shell-ready")[..3].to_vec(),
        ] {
            let error = send_marker(&marker).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{marker:?}");
        }
        assert!(send_marker(&args(missing, NONCE, "shell-ready")).is_err());
    }
}
