use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use flate2::read::{GzDecoder, ZlibDecoder};
use ofx_contract::BoxFuture;
use ofx_http::{ConnectionOptions, certificate_bundle_load_failure, connection_client_builder};
use reqwest::Proxy;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, LOCATION};
use rustls::CertificateError;
use tokio_util::sync::CancellationToken;

use super::url_policy::{Redirect, ValidatedUrl, is_public_address, redirect_target};

pub(crate) const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_REDIRECT_HOPS: usize = 10;
const HOP_TIMEOUT: Duration = Duration::from_mins(1);
const USER_AGENT: &str = "oh-fx (web_fetch; +https://github.com/binbandit/oh-fx)";
const ACCEPT: &str = "text/markdown, text/html, */*";
const ACCEPT_ENCODING: &str = "gzip, deflate";
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

type Lookup = fn(String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>>;
type Answer = Result<Vec<SocketAddr>, ResolveFailure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransportError(pub(crate) &'static str);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) final_url: String,
    pub(crate) status: u16,
    pub(crate) content_type: Option<String>,
    content_encodings: Vec<String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Retrieved {
    Response(Response),
    CrossHostRedirect(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settled {
    Success { body: Vec<u8> },
    NonSuccessStatus { body: Vec<u8> },
    UnexpectedContentEncoding { encoding: String },
}

#[derive(Debug, Clone)]
pub(crate) struct Transport {
    proxy: Option<Proxy>,
    ca_file: Option<PathBuf>,
    lookup: Lookup,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            proxy: None,
            ca_file: None,
            lookup: system_lookup,
        }
    }
}

impl Transport {
    #[cfg(test)]
    pub(crate) fn through(proxy: Proxy, ca_file: PathBuf, lookup: Lookup) -> Self {
        Self {
            proxy: Some(proxy),
            ca_file: Some(ca_file),
            lookup,
        }
    }

    pub(crate) async fn fetch(
        &self,
        initial: &ValidatedUrl,
        cancel: &CancellationToken,
    ) -> Result<Retrieved, TransportError> {
        let resolver = Arc::new(PinnedResolver::default());
        let client = self.client(&resolver)?;
        let mut current = initial.clone();
        for _ in 0..=MAX_REDIRECT_HOPS {
            let hop = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(TransportError("Canceled")),
                hop = tokio::time::timeout(HOP_TIMEOUT, self.hop(&client, &resolver, &current)) => hop,
            };
            match hop.map_err(|_| TransportError("Timeout"))?? {
                Hop::Final(response) => return Ok(Retrieved::Response(response)),
                Hop::Redirect(location) => {
                    match redirect_target(&current, &location)
                        .map_err(|error| TransportError(error.name()))?
                    {
                        Redirect::Follow(target) => current = target,
                        Redirect::CrossHost(url) => return Ok(Retrieved::CrossHostRedirect(url)),
                    }
                }
            }
        }
        Err(TransportError("RedirectLimitExceeded"))
    }

    async fn hop(
        &self,
        client: &reqwest::Client,
        resolver: &PinnedResolver,
        url: &ValidatedUrl,
    ) -> Result<Hop, TransportError> {
        let answer = self.answer(&url.canonical_host).await;
        if answer == Err(ResolveFailure::NonPublicDnsAnswer) {
            return Err(TransportError(ResolveFailure::NonPublicDnsAnswer.name()));
        }
        resolver.pin(&url.canonical_host, answer);
        request(client, url).await
    }

    async fn answer(&self, host: &str) -> Answer {
        if let Ok(address) = host.parse::<IpAddr>() {
            return admitted(vec![SocketAddr::new(address, 0)]);
        }
        (self.lookup)(host.to_owned())
            .await
            .map_or(Err(ResolveFailure::UnknownHostName), admitted)
    }

    fn client(&self, resolver: &Arc<PinnedResolver>) -> Result<reqwest::Client, TransportError> {
        let options = ConnectionOptions {
            user_agent: USER_AGENT.to_owned(),
            default_headers: vec![
                ("Accept".to_owned(), ACCEPT.to_owned()),
                ("Accept-Encoding".to_owned(), ACCEPT_ENCODING.to_owned()),
                ("Connection".to_owned(), "close".to_owned()),
            ],
            ca_file: self.ca_file.clone(),
            proxy: None,
            follow_redirects: false,
        };
        let mut builder = connection_client_builder(&options)
            .map_err(|_| TransportError("TlsInitializationFailed"))?
            .connect_timeout(HOP_TIMEOUT)
            .dns_resolver(Arc::clone(resolver));
        if let Some(proxy) = &self.proxy {
            builder = builder.proxy(proxy.clone());
        }
        builder
            .build()
            .map_err(|_| TransportError("TlsInitializationFailed"))
    }
}

pub(crate) fn settle(response: Response) -> Result<Settled, TransportError> {
    let Response {
        status,
        content_encodings,
        body,
        ..
    } = response;
    let success = (200..300).contains(&status);
    let Some(encoding) = content_encodings.first() else {
        return Ok(plain(success, body));
    };
    let unexpected = || Settled::UnexpectedContentEncoding {
        encoding: encoding.clone(),
    };
    if content_encodings.len() > 1 {
        return Ok(unexpected());
    }
    let coding = if encoding.eq_ignore_ascii_case("identity") {
        return Ok(plain(success, body));
    } else if encoding.eq_ignore_ascii_case("gzip") {
        Coding::Gzip
    } else if encoding.eq_ignore_ascii_case("deflate") {
        Coding::Deflate
    } else {
        return Ok(unexpected());
    };
    if !success {
        return Ok(unexpected());
    }
    match decode(coding, &body) {
        Ok(decoded) if decoded.len() > MAX_BODY_BYTES => Err(TransportError("BodyTooLarge")),
        Ok(decoded) => Ok(Settled::Success { body: decoded }),
        Err(_) => Ok(unexpected()),
    }
}

fn plain(success: bool, body: Vec<u8>) -> Settled {
    if success {
        Settled::Success { body }
    } else {
        Settled::NonSuccessStatus { body }
    }
}

#[derive(Debug, Clone, Copy)]
enum Coding {
    Gzip,
    Deflate,
}

fn decode(coding: Coding, encoded: &[u8]) -> io::Result<Vec<u8>> {
    let reader: Box<dyn Read + '_> = match coding {
        Coding::Gzip => Box::new(GzDecoder::new(encoded)),
        Coding::Deflate => Box::new(ZlibDecoder::new(encoded)),
    };
    let mut decoded = Vec::new();
    reader
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut decoded)?;
    Ok(decoded)
}

enum Hop {
    Final(Response),
    Redirect(String),
}

async fn request(client: &reqwest::Client, url: &ValidatedUrl) -> Result<Hop, TransportError> {
    let mut response = client
        .get(&url.retrieval_url)
        .send()
        .await
        .map_err(|error| transport_error(&error))?;
    let status = response.status().as_u16();
    let headers = response.headers();
    if REDIRECT_STATUSES.contains(&status) {
        let location =
            last_header(headers, &LOCATION).ok_or(TransportError("MissingRedirectLocation"))?;
        return Ok(Hop::Redirect(location));
    }
    let content_type = last_header(headers, &CONTENT_TYPE);
    let content_encodings = headers
        .get_all(CONTENT_ENCODING)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(TransportError("BodyTooLarge"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| transport_error(&error))?
    {
        if chunk.len() > MAX_BODY_BYTES - body.len() {
            return Err(TransportError("BodyTooLarge"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Hop::Final(Response {
        final_url: url.retrieval_url.clone(),
        status,
        content_type,
        content_encodings,
        body,
    }))
}

fn last_header(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get_all(name)
        .iter()
        .last()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
}

fn system_lookup(host: String) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
    Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.collect()) })
}

#[derive(Debug, Default)]
struct PinnedResolver {
    pinned: Mutex<Option<(String, Answer)>>,
}

impl PinnedResolver {
    fn pin(&self, host: &str, answer: Answer) {
        *self.pinned.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((host.to_owned(), answer));
    }

    fn answer(&self, host: &str) -> Option<Answer> {
        self.pinned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(pinned, _)| pinned.eq_ignore_ascii_case(host))
            .map(|(_, answer)| answer.clone())
    }
}

impl Resolve for PinnedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let pinned = self.answer(name.as_str());
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = match pinned {
                Some(answer) => answer?,
                None => system_lookup(host)
                    .await
                    .map_err(|_| ResolveFailure::UnknownHostName)?,
            };
            Ok::<Addrs, Box<dyn StdError + Send + Sync>>(Box::new(addresses.into_iter()))
        })
    }
}

fn admitted(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, ResolveFailure> {
    if addresses.is_empty() {
        return Err(ResolveFailure::NoAddressReturned);
    }
    if addresses
        .iter()
        .any(|address| !is_public_address(address.ip()))
    {
        return Err(ResolveFailure::NonPublicDnsAnswer);
    }
    Ok(addresses)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveFailure {
    UnknownHostName,
    NoAddressReturned,
    NonPublicDnsAnswer,
}

impl ResolveFailure {
    fn name(self) -> &'static str {
        match self {
            Self::UnknownHostName => "UnknownHostName",
            Self::NoAddressReturned => "NoAddressReturned",
            Self::NonPublicDnsAnswer => "NonPublicDnsAnswer",
        }
    }
}

impl fmt::Display for ResolveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl StdError for ResolveFailure {}

fn transport_error(error: &reqwest::Error) -> TransportError {
    if certificate_bundle_load_failure(error).is_some() {
        return TransportError("CertificateBundleLoadFailure");
    }
    let mut next: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(cause) = next {
        if let Some(name) = cause_name(cause) {
            return TransportError(name);
        }
        if let Some(io_error) = cause.downcast_ref::<io::Error>() {
            if let Some(name) = io_error.get_ref().and_then(|inner| cause_name(inner)) {
                return TransportError(name);
            }
            if let Some(name) = io_error_name(io_error.kind()) {
                return TransportError(name);
            }
        }
        next = cause.source();
    }
    TransportError(if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "ConnectionFailed"
    } else if error.is_body() || error.is_decode() {
        "ReadFailed"
    } else {
        "RequestFailed"
    })
}

fn cause_name(cause: &(dyn StdError + 'static)) -> Option<&'static str> {
    if let Some(failure) = cause.downcast_ref::<ResolveFailure>() {
        return Some(failure.name());
    }
    cause.downcast_ref::<rustls::Error>().map(tls_error_name)
}

fn tls_error_name(error: &rustls::Error) -> &'static str {
    match error {
        rustls::Error::InvalidCertificate(certificate) => match certificate {
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                "CertificateHostMismatch"
            }
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                "CertificateExpired"
            }
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                "CertificateNotYetValid"
            }
            CertificateError::UnknownIssuer => "CertificateIssuerNotFound",
            CertificateError::BadSignature => "CertificateSignatureInvalid",
            _ => "TlsCertificateNotVerified",
        },
        rustls::Error::AlertReceived(_) => "TlsAlert",
        _ => "TlsInitializationFailed",
    }
}

fn io_error_name(kind: io::ErrorKind) -> Option<&'static str> {
    Some(match kind {
        io::ErrorKind::ConnectionRefused => "ConnectionRefused",
        io::ErrorKind::ConnectionReset => "ConnectionResetByPeer",
        io::ErrorKind::ConnectionAborted => "ConnectionAborted",
        io::ErrorKind::TimedOut => "ConnectionTimedOut",
        io::ErrorKind::HostUnreachable => "HostUnreachable",
        io::ErrorKind::NetworkUnreachable => "NetworkUnreachable",
        io::ErrorKind::NetworkDown => "NetworkDown",
        io::ErrorKind::UnexpectedEof => "UnexpectedClose",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
