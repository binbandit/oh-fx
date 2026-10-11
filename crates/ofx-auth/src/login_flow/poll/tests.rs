use std::collections::VecDeque;

use super::*;
use crate::secret::Secret;

#[derive(Debug, Clone, Copy)]
enum Scripted {
    Pending,
    SlowDown,
    Success,
}

#[derive(Default)]
struct ScriptedPoll {
    results: VecDeque<Scripted>,
    polls: usize,
    sleeps: Vec<u64>,
    waits: Vec<u64>,
    enter_on_wait: Option<usize>,
    opened: Vec<String>,
    open_available: bool,
    now_ms: i64,
}

impl ScriptedPoll {
    fn new(results: &[Scripted]) -> Self {
        Self {
            results: results.iter().copied().collect(),
            open_available: true,
            ..Self::default()
        }
    }
}

impl PollDeps for ScriptedPoll {
    fn now_ms(&self) -> i64 {
        self.now_ms
    }

    async fn poll_device_token(&mut self, device_code: &str) -> Result<PollResult, VercelError> {
        assert_eq!(device_code, "device");
        self.polls += 1;
        match self.results.pop_front().expect("an unexpected device poll") {
            Scripted::Pending => Ok(PollResult::Pending),
            Scripted::SlowDown => Ok(PollResult::SlowDown),
            Scripted::Success => Ok(PollResult::Success(TokenSet {
                access_token: Secret::new("access".to_owned()),
                refresh_token: Some(Secret::new("refresh".to_owned())),
                expires_in: 3600,
                scope: "openid offline_access".to_owned(),
                token_type: "Bearer".to_owned(),
            })),
        }
    }

    async fn sleep_ms(&mut self, milliseconds: u64) {
        self.sleeps.push(milliseconds);
        self.now_ms += i64::try_from(milliseconds).unwrap();
    }

    fn wait_for_enter(&mut self, timeout_ms: u64) -> bool {
        self.waits.push(timeout_ms);
        self.now_ms += i64::try_from(timeout_ms).unwrap();
        self.enter_on_wait == Some(self.waits.len())
    }

    fn open_url(&mut self, url: &str) -> bool {
        self.opened.push(url.to_owned());
        self.open_available
    }
}

fn device() -> DeviceAuthorization {
    DeviceAuthorization {
        device_code: Secret::new("device".to_owned()),
        user_code: "USER-CODE".to_owned(),
        verification_uri: "https://vercel.test/oauth/device".to_owned(),
        verification_uri_complete: None,
        expires_in: 60,
        interval: 1,
    }
}

async fn run(deps: &mut ScriptedPoll, device: &DeviceAuthorization, enabled: bool) -> bool {
    let mut prompt = BrowserOpenPrompt::new("https://vercel.test/oauth/device", enabled);
    let token = poll_for_token(deps, device, &mut prompt).await.unwrap();
    assert_eq!(token.access_token.expose(), "access");
    prompt.opened
}

#[tokio::test]
async fn login_polling_starts_before_waiting_for_browser_input() {
    let mut deps = ScriptedPoll::new(&[Scripted::Success]);
    run(&mut deps, &device(), true).await;
    assert_eq!(deps.polls, 1);
    assert!(deps.waits.is_empty());
    assert!(deps.sleeps.is_empty());
    assert!(deps.opened.is_empty());
}

#[tokio::test]
async fn login_polling_opens_the_browser_once_when_enter_arrives_while_waiting() {
    let mut deps = ScriptedPoll::new(&[Scripted::Pending, Scripted::Pending, Scripted::Success]);
    deps.enter_on_wait = Some(1);
    assert!(run(&mut deps, &device(), true).await);
    assert_eq!(deps.polls, 3);
    assert_eq!(deps.opened, ["https://vercel.test/oauth/device"]);
    assert_eq!(deps.waits, [100]);
    assert_eq!(deps.sleeps, [100; 19]);
}

#[tokio::test]
async fn login_polling_continues_when_the_url_opener_is_unavailable() {
    let mut deps = ScriptedPoll::new(&[Scripted::Pending, Scripted::Pending, Scripted::Success]);
    deps.enter_on_wait = Some(1);
    deps.open_available = false;
    assert!(run(&mut deps, &device(), true).await);
    assert_eq!(deps.polls, 3);
    assert_eq!(deps.opened.len(), 1);
}

#[tokio::test]
async fn login_polling_slow_down_increases_the_next_interval() {
    let mut deps = ScriptedPoll::new(&[Scripted::SlowDown, Scripted::Success]);
    run(&mut deps, &device(), false).await;
    assert_eq!(deps.polls, 2);
    assert_eq!(deps.sleeps, [100; 60]);
}

#[tokio::test]
async fn a_disabled_browser_prompt_still_polls_immediately() {
    let mut deps = ScriptedPoll::new(&[Scripted::Pending, Scripted::Success]);
    deps.enter_on_wait = Some(1);
    assert!(!run(&mut deps, &device(), false).await);
    assert_eq!(deps.polls, 2);
    assert!(deps.waits.is_empty());
    assert!(deps.opened.is_empty());
    assert_eq!(deps.sleeps, [100; 10]);
}

#[tokio::test]
async fn device_code_expiry_ends_polling_without_another_request() {
    let mut deps = ScriptedPoll::new(&[Scripted::Pending]);
    let device = DeviceAuthorization {
        expires_in: 1,
        ..device()
    };
    let mut prompt = BrowserOpenPrompt::new("https://vercel.test/oauth/device", false);
    assert_eq!(
        poll_for_token(&mut deps, &device, &mut prompt).await,
        Err(VercelError::LoginTimedOut)
    );
    assert_eq!(deps.polls, 1);
}

#[tokio::test]
async fn login_polling_rejects_invalid_provider_timing_values() {
    let mut deps = ScriptedPoll::new(&[]);
    let mut prompt = BrowserOpenPrompt::new("https://vercel.test/oauth/device", false);
    for device in [
        DeviceAuthorization {
            expires_in: -1,
            ..device()
        },
        DeviceAuthorization {
            interval: i64::try_from(MAX_POLL_INTERVAL_MS / 1000 + 1).unwrap(),
            ..device()
        },
    ] {
        assert_eq!(
            poll_for_token(&mut deps, &device, &mut prompt).await,
            Err(VercelError::InvalidOAuthResponse)
        );
    }
    assert_eq!(deps.polls, 0);
}

#[test]
fn the_waiting_message_names_the_browser_prompt_only_when_it_is_enabled() {
    assert_eq!(
        BrowserOpenPrompt::new("u", true).waiting_message(),
        "Waiting for authentication. press enter to open this in your browser, or use the URL manually.\n"
    );
    assert_eq!(
        BrowserOpenPrompt::new("u", false).waiting_message(),
        "Waiting for authentication...\n"
    );
}

#[test]
fn enter_on_the_input_opens_the_browser_after_discarding_the_line() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let (input, mut writer) = UnixStream::pair().unwrap();
    let transport = Transport::new("oh-fx/test").unwrap();
    let metadata = Metadata {
        issuer: String::new(),
        device_authorization_endpoint: String::new(),
        token_endpoint: String::new(),
        revocation_endpoint: None,
    };
    let mut deps = TerminalPoll {
        transport: &transport,
        metadata: &metadata,
        client_id: "client",
        input: &input,
    };
    assert!(!deps.wait_for_enter(10));
    writer.write_all(b"typed\n").unwrap();
    assert!(deps.wait_for_enter(10));
    assert!(!deps.wait_for_enter(10));
}
