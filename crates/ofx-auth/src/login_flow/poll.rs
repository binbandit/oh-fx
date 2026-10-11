use std::os::fd::AsFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::VercelError;
use crate::oauth::device::{self, DeviceAuthorization, Metadata, PollResult, TokenSet};
use crate::oauth::expiry_timestamp_ms;
use crate::oauth_transport::Transport;
use crate::subscription_access::now_ms;

const WAIT_SLICE_MS: u64 = 100;
const SLOW_DOWN_STEP_MS: u64 = 5_000;
const MILLISECONDS_PER_SECOND: i64 = 1_000;
const MAX_POLL_INTERVAL_MS: u64 = u64::MAX / 1_000_000;
const WAITING_WITH_PROMPT: &str = "Waiting for authentication. press enter to open this in your browser, or use the URL manually.\n";
const WAITING: &str = "Waiting for authentication...\n";

pub(super) struct BrowserOpenPrompt<'a> {
    url: &'a str,
    enabled: bool,
    opened: bool,
}

impl<'a> BrowserOpenPrompt<'a> {
    pub(super) fn new(url: &'a str, enabled: bool) -> Self {
        Self {
            url,
            enabled,
            opened: false,
        }
    }

    pub(super) fn waiting_message(&self) -> &'static str {
        if self.enabled {
            WAITING_WITH_PROMPT
        } else {
            WAITING
        }
    }
}

pub(super) trait PollDeps {
    fn now_ms(&self) -> i64;
    async fn poll_device_token(&mut self, device_code: &str) -> Result<PollResult, VercelError>;
    async fn sleep_ms(&mut self, milliseconds: u64);
    fn wait_for_enter(&mut self, timeout_ms: u64) -> bool;
    fn open_url(&mut self, url: &str) -> bool;
}

pub(super) struct TerminalPoll<'a, F> {
    pub(super) transport: &'a Transport,
    pub(super) metadata: &'a Metadata,
    pub(super) client_id: &'a str,
    pub(super) input: &'a F,
}

impl<F: AsFd> PollDeps for TerminalPoll<'_, F> {
    fn now_ms(&self) -> i64 {
        now_ms()
    }

    async fn poll_device_token(&mut self, device_code: &str) -> Result<PollResult, VercelError> {
        device::poll_device_token(self.transport, self.metadata, self.client_id, device_code).await
    }

    async fn sleep_ms(&mut self, milliseconds: u64) {
        tokio::time::sleep(Duration::from_millis(milliseconds)).await;
    }

    fn wait_for_enter(&mut self, timeout_ms: u64) -> bool {
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: i64::try_from(timeout_ms.min(999) * 1_000_000).unwrap_or(0),
        };
        let mut descriptors = [PollFd::new(self.input, PollFlags::IN)];
        let ready = poll(&mut descriptors, Some(&timeout)).unwrap_or(0);
        if ready == 0 || !descriptors[0].revents().contains(PollFlags::IN) {
            return false;
        }
        discard_line(self.input);
        true
    }

    fn open_url(&mut self, url: &str) -> bool {
        crate::url_opener::open_url(url)
    }
}

fn discard_line<F: AsFd>(input: &F) {
    let mut buffer = [0_u8; 256];
    while let Ok(count) = rustix::io::read(input, &mut buffer) {
        if count == 0 || buffer[..count].contains(&b'\n') {
            return;
        }
    }
}

struct PollState {
    expires_at: i64,
    interval: u64,
    next_poll_at: i64,
}

impl PollState {
    fn new(now_ms: i64, device: &DeviceAuthorization) -> Result<Self, VercelError> {
        Ok(Self {
            expires_at: expiry_timestamp_ms(now_ms, device.expires_in)?,
            interval: poll_interval_ms(device.interval)?,
            next_poll_at: now_ms,
        })
    }

    fn schedule_next(&mut self, now_ms: i64) -> Result<(), VercelError> {
        self.next_poll_at = i64::try_from(self.interval)
            .ok()
            .and_then(|interval| now_ms.checked_add(interval))
            .ok_or(VercelError::InvalidOAuthResponse)?;
        Ok(())
    }

    fn wait_ms(&self, now_ms: i64) -> u64 {
        u64::try_from(self.next_poll_at.saturating_sub(now_ms)).unwrap_or(0)
    }
}

fn poll_interval_ms(interval_seconds: i64) -> Result<u64, VercelError> {
    interval_seconds
        .max(1)
        .checked_mul(MILLISECONDS_PER_SECOND)
        .and_then(|milliseconds| u64::try_from(milliseconds).ok())
        .filter(|milliseconds| *milliseconds <= MAX_POLL_INTERVAL_MS)
        .ok_or(VercelError::InvalidOAuthResponse)
}

pub(super) async fn poll_for_token<D: PollDeps>(
    deps: &mut D,
    device: &DeviceAuthorization,
    prompt: &mut BrowserOpenPrompt<'_>,
) -> Result<TokenSet, VercelError> {
    let mut state = PollState::new(deps.now_ms(), device)?;
    loop {
        if let Some(token) = poll_token_step(deps, device, &mut state).await? {
            return Ok(token);
        }
        let wait_ms = state.wait_ms(deps.now_ms());
        if wait_ms > 0 {
            wait_between_polls(deps, prompt, wait_ms).await;
        }
    }
}

async fn poll_token_step<D: PollDeps>(
    deps: &mut D,
    device: &DeviceAuthorization,
    state: &mut PollState,
) -> Result<Option<TokenSet>, VercelError> {
    let now_ms = deps.now_ms();
    if now_ms >= state.expires_at {
        return Err(VercelError::LoginTimedOut);
    }
    if now_ms < state.next_poll_at {
        return Ok(None);
    }
    match deps.poll_device_token(device.device_code.expose()).await? {
        PollResult::Success(token) => return Ok(Some(token)),
        PollResult::Pending => {}
        PollResult::SlowDown => {
            state.interval = state
                .interval
                .checked_add(SLOW_DOWN_STEP_MS)
                .filter(|interval| *interval <= MAX_POLL_INTERVAL_MS)
                .ok_or(VercelError::InvalidOAuthResponse)?;
        }
    }
    state.schedule_next(deps.now_ms())?;
    Ok(None)
}

async fn wait_between_polls<D: PollDeps>(
    deps: &mut D,
    prompt: &mut BrowserOpenPrompt<'_>,
    interval_ms: u64,
) {
    let mut remaining_ms = interval_ms;
    while remaining_ms > 0 {
        let slice_ms = remaining_ms.min(WAIT_SLICE_MS);
        if prompt.enabled && !prompt.opened {
            if deps.wait_for_enter(slice_ms) {
                prompt.opened = true;
                deps.open_url(prompt.url);
            }
        } else {
            deps.sleep_ms(slice_ms).await;
        }
        remaining_ms -= slice_ms;
    }
}

#[cfg(test)]
mod tests;
