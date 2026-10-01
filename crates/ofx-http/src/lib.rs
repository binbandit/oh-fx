#[cfg(target_os = "linux")]
mod ca_bundle;
mod client;
mod sse;

pub use client::{
    ClientError, ConnectionOptions, build_connection_client, certificate_bundle_load_failure,
    warm_tls_roots,
};
pub use sse::{SseDecoder, SseError};
