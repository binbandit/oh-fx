use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::io::Write;
use std::os::fd::AsFd;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

use super::{GrokError, GrokOAuth};
use crate::secret::Secret;
use tokio_util::sync::CancellationToken;

impl GrokOAuth {
    pub async fn run_login(
        &self,
        output: &mut (dyn Write + Send),
        open_browser: bool,
        cancel: &CancellationToken,
    ) -> Result<(), GrokError> {
        self.run_login_with_input(output, open_browser, cancel, &std::io::stdin())
            .await
    }

    async fn run_login_with_input<F: AsFd>(
        &self,
        output: &mut (dyn Write + Send),
        open_browser: bool,
        cancel: &CancellationToken,
        input: &F,
    ) -> Result<(), GrokError> {
        let sign_in = self.start_sign_in().await?;
        let url = sign_in.authorization_url();
        write!(output, "Open this URL to sign in with Grok:\n{url}\n\nWaiting for browser authorization...\nPaste the code shown by xAI and press enter if the browser doesn't return.\n")
            .and_then(|()| output.flush())
            .map_err(|_| GrokError::WriteFailed)?;
        if open_browser {
            crate::url_opener::open_url(url);
        }
        let completion = sign_in.finish(cancel);
        tokio::pin!(completion);
        let mut reader = ManualCodeReader::default();
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                biased;
                result = &mut completion => return result,
                _ = interval.tick() => {
                    if let Some(code) = reader.poll(input)? {
                        sign_in.submit_manual_code(code.expose())?;
                    }
                }
            }
        }
    }
}

struct ManualCodeReader {
    buffer: Zeroizing<[u8; 4096]>,
    len: usize,
    closed: bool,
}

impl Default for ManualCodeReader {
    fn default() -> Self {
        Self {
            buffer: Zeroizing::new([0; 4096]),
            len: 0,
            closed: false,
        }
    }
}

impl ManualCodeReader {
    fn poll<F: AsFd>(&mut self, input: &F) -> Result<Option<Secret>, GrokError> {
        if self.closed {
            return Ok(None);
        }
        let mut descriptors = [PollFd::new(input, PollFlags::IN)];
        if poll(&mut descriptors, Some(&Timespec::default())).map_err(|_| GrokError::ReadFailed)?
            == 0
        {
            return Ok(None);
        }
        if !descriptors[0]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP)
        {
            return Ok(None);
        }
        let mut chunk = Zeroizing::new([0; 512]);
        let count =
            rustix::io::read(input, chunk.as_mut_slice()).map_err(|_| GrokError::ReadFailed)?;
        if count == 0 {
            self.closed = true;
            return if self.len == 0 {
                Ok(None)
            } else {
                self.take_code()
            };
        }
        let end = chunk[..count]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(count);
        if end > self.buffer.len() - self.len {
            return Err(GrokError::GrokAuthorizationCodeTooLong);
        }
        self.buffer[self.len..self.len + end].copy_from_slice(&chunk[..end]);
        self.len += end;
        if end < count {
            self.closed = true;
            return self.take_code();
        }
        Ok(None)
    }

    fn take_code(&mut self) -> Result<Option<Secret>, GrokError> {
        let code = std::str::from_utf8(&self.buffer[..self.len])
            .map_err(|_| GrokError::InvalidGrokAuthorizationCode)?;
        let code = Secret::new(code.to_owned());
        self.buffer[..self.len].zeroize();
        self.len = 0;
        Ok(Some(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ofx_testkit::{FakeServer, Reply};
    use std::os::unix::net::UnixStream;

    #[test]
    fn manual_code_reader_accepts_first_line_and_eof_without_waiting() {
        for newline in [false, true] {
            let (input, mut writer) = UnixStream::pair().unwrap();
            writer
                .write_all(if newline { b"code\nignored" } else { b"code" })
                .unwrap();
            drop(writer);
            let mut reader = ManualCodeReader::default();
            let code = loop {
                if let Some(code) = reader.poll(&input).unwrap() {
                    break code;
                }
            };
            assert_eq!(code.expose(), "code");
            assert!(reader.poll(&input).unwrap().is_none());
        }
    }

    #[test]
    fn manual_code_reader_distinguishes_empty_line_from_empty_eof() {
        for newline in [false, true] {
            let (input, mut writer) = UnixStream::pair().unwrap();
            if newline {
                writer.write_all(b"\n").unwrap();
            }
            drop(writer);
            let mut reader = ManualCodeReader::default();
            let result = reader.poll(&input).unwrap();
            if newline {
                assert_eq!(result.unwrap().expose(), "");
            } else {
                assert!(result.is_none());
            }
        }
    }

    #[test]
    fn manual_code_reader_bounds_partial_input_and_keeps_exact_boundary() {
        for count in [4096, 4097] {
            let (input, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&vec![b'x'; count]).unwrap();
            drop(writer);
            let mut reader = ManualCodeReader::default();
            let result = loop {
                match reader.poll(&input) {
                    Ok(None) => {}
                    result => break result,
                }
            };
            if count == 4096 {
                assert_eq!(result.unwrap().unwrap().expose().len(), count);
            } else {
                assert_eq!(result.err(), Some(GrokError::GrokAuthorizationCodeTooLong));
            }
        }
    }

    #[tokio::test]
    async fn run_login_rejects_invalid_stdin_before_any_token_request() {
        for (input_bytes, expected) in [
            (b"\n".to_vec(), GrokError::InvalidGrokAuthorizationCode),
            (vec![b'x'; 4097], GrokError::GrokAuthorizationCodeTooLong),
        ] {
            let server = FakeServer::start([]);
            let directory = tempfile::tempdir().unwrap();
            let oauth = GrokOAuth::new(
                directory.path().join("profile"),
                "oh-fx/test",
                super::super::GrokEndpoints {
                    issuer: server.base_url(),
                    token_url: format!("{}/token", server.base_url()),
                    userinfo_url: format!("{}/userinfo", server.base_url()),
                    revoke_url: format!("{}/revoke", server.base_url()),
                },
            )
            .unwrap();
            let (input, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&input_bytes).unwrap();
            drop(writer);
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                oauth.run_login_with_input(
                    &mut Vec::new(),
                    false,
                    &CancellationToken::new(),
                    &input,
                ),
            )
            .await
            .unwrap();
            assert_eq!(result, Err(expected));
            assert!(server.requests().is_empty());
            assert!(oauth.store.load().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn run_login_cancellation_never_waits_for_an_idle_stdin_writer() {
        let directory = tempfile::tempdir().unwrap();
        let oauth = GrokOAuth::new(
            directory.path().join("profile"),
            "oh-fx/test",
            super::super::GrokEndpoints::default(),
        )
        .unwrap();
        let (input, _writer) = UnixStream::pair().unwrap();
        let cancel = CancellationToken::new();
        let interrupt = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        };
        let mut output = Vec::new();
        let finished = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                oauth.run_login_with_input(&mut output, false, &cancel, &input),
                interrupt
            )
        })
        .await
        .unwrap();
        assert_eq!(finished.0, Err(GrokError::Cancelled));
        assert!(oauth.store.load().unwrap().is_none());
    }

    #[tokio::test]
    async fn run_login_prints_upstream_messages_and_exchanges_manual_stdin() {
        let server = FakeServer::start([
            Reply::status(
                200,
                r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
            ),
            Reply::status(200, r#"{"sub":"acct_test"}"#),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let oauth = GrokOAuth::new(
            directory.path().join("profile"),
            "oh-fx/test",
            super::super::GrokEndpoints {
                issuer: server.base_url(),
                token_url: format!("{}/token", server.base_url()),
                userinfo_url: format!("{}/userinfo", server.base_url()),
                revoke_url: format!("{}/revoke", server.base_url()),
            },
        )
        .unwrap();
        let (input, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"manual-code\n").unwrap();
        let mut output = Vec::new();
        oauth
            .run_login_with_input(&mut output, false, &CancellationToken::new(), &input)
            .await
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("Open this URL to sign in with Grok:\n"));
        assert!(output.ends_with("\n\nWaiting for browser authorization...\nPaste the code shown by xAI and press enter if the browser doesn't return.\n"));
        assert_eq!(server.requests().len(), 2);
        assert!(String::from_utf8_lossy(&server.requests()[0].body).contains("code=manual-code"));
        assert!(oauth.store.load().unwrap().is_some());
    }
}
