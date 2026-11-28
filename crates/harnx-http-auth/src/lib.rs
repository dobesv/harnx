//! OAuth service-account credentials and HTTP client helpers. Consumers own
//! destination policy and decide whether a rejected request can be replayed.

mod client_credentials;
pub mod net;
#[doc(hidden)]
pub mod oauth;
pub mod redact;
pub mod tls;

pub use client_credentials::{AccessToken, ClientCredentialsProvider};
