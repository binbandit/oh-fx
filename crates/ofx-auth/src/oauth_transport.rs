use std::time::Duration;

use ofx_http::{ConnectionOptions, build_connection_client};
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use zeroize::Zeroizing;

pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    PostForm,
    PostJson,
}

impl Method {
    const fn content_type(self) -> &'static str {
        match self {
            Self::PostForm => "application/x-www-form-urlencoded",
            Self::PostJson => "application/json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TransportError {
    #[error("OAuthTransportUnavailable")]
    Unavailable,
    #[error("ConnectionFailed")]
    ConnectionFailed,
    #[error("Timeout")]
    Timeout,
    #[error("OAuthResponseTooLarge")]
    ResponseTooLarge,
}

pub(crate) struct Response {
    pub(crate) accepted: bool,
    pub(crate) body: Zeroizing<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct Transport {
    client: reqwest::Client,
}

impl Transport {
    pub(crate) fn new(user_agent: &str) -> Result<Self, TransportError> {
        let client = build_connection_client(&ConnectionOptions {
            user_agent: user_agent.to_owned(),
            follow_redirects: false,
            ..ConnectionOptions::default()
        })
        .map_err(|_| TransportError::Unavailable)?;
        Ok(Self { client })
    }

    pub(crate) async fn execute(
        &self,
        method: Method,
        url: &str,
        payload: &str,
    ) -> Result<Response, TransportError> {
        let request = self
            .client
            .post(url)
            .header(CONTENT_TYPE, method.content_type())
            .body(payload.to_owned());
        self.execute_request(request).await
    }

    pub(crate) async fn execute_authorized_get(
        &self,
        url: &str,
        token: &str,
    ) -> Result<Response, TransportError> {
        self.execute_request(self.client.get(url).bearer_auth(token))
            .await
    }

    async fn execute_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Response, TransportError> {
        let exchange = async {
            let mut response = request.send().await.map_err(|error| {
                if error.is_timeout() {
                    TransportError::Timeout
                } else {
                    TransportError::ConnectionFailed
                }
            })?;
            let accepted = response.status() == StatusCode::OK;
            let mut body = Zeroizing::new(Vec::with_capacity(MAX_RESPONSE_BYTES));
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| TransportError::ConnectionFailed)?
            {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(TransportError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Response { accepted, body })
        };
        tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| TransportError::Timeout)?
    }
}
