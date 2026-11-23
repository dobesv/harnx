//! Encrypted tool credentials, single-use connect flows, refresh leases and catalogs.
//!
//! Each server loads its own [`Keyring`] and opens a [`CredentialStore`] for a
//! canonical [`Principal`]. All mutations use KV revisions, including tombstones:
//! read [`CredentialStore::inspect`] before a connect, then pass its revision to
//! [`CredentialStore::put_connect`]. Refresh through [`RefreshLease::commit`].
//! `connection_id` identifies a connection, not a token revision. Derived catalogs
//! and registrations must keep that ID and reject updates from older connections.
//!
//! Secrets serialize for encrypted persistence, but never print. Only explicitly
//! call [`Secret::expose`] at the HTTP/OAuth boundary. Don't log serialized records.

mod backend;
pub mod catalog;
pub mod envelope;
pub mod flow;
pub mod keyring;
pub mod lease;
pub mod model;
pub mod rmcp_adapters;
pub mod store;

pub use catalog::{Catalog, CatalogStore};
pub use flow::{FlowOptions, FlowStatus, PendingFlow, PendingFlowStore, FLOW_TTL};
pub use keyring::Keyring;
pub use lease::{RefreshLease, RefreshLeaseStore, REFRESH_LEASE_TTL};
pub use model::{
    ClientRegistration, CredentialMaterial, CredentialRecord, CredentialStatus, NewCredential,
    OAuthMaterial, Principal, Secret, TokenMaterial,
};
pub use store::{CredentialStore, Snapshot, Versioned, DEFAULT_CREDENTIAL_TTL};

/// Errors deliberately exclude plaintext payloads and provider error bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// Key file could not be read (including a missing file).
    KeyFileIo,
    /// Invalid JSON, active key, or key length.
    InvalidKeyFile,
    /// Envelope was malformed or used an unsupported version/algorithm.
    InvalidEnvelope,
    /// Envelope refers to a key absent from this server's keyring.
    UnknownKey,
    /// AEAD authentication failed. No plaintext is returned.
    AuthenticationFailed,
    /// Operating system randomness or encryption failed.
    Crypto,
    /// KV revision no longer matches the caller's snapshot.
    Conflict,
    /// Absolute expiry passed; cleanup TTL is not an authorization check.
    Expired,
    /// Credential requires a new connect flow.
    NeedsLogin,
    /// Requested record does not exist.
    NotFound,
    /// Flow no longer allows this transition.
    InvalidTransition,
    /// Another refresher owns the lease.
    LeaseBusy,
    /// Lease expired, was released, or was replaced.
    LeaseLost,
    /// Derived state belongs to a different connection.
    ConnectionMismatch,
    /// Invalid identity, schema, or material.
    InvalidRecord,
    /// NATS operation failed. No backend payload is included in this error.
    Storage,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::KeyFileIo => "cannot read credentials key file",
            Self::InvalidKeyFile => "invalid credentials key file",
            Self::InvalidEnvelope => "invalid encrypted envelope",
            Self::UnknownKey => "unknown encryption key",
            Self::AuthenticationFailed => "encrypted envelope authentication failed",
            Self::Crypto => "cryptographic operation failed",
            Self::Conflict => "credential storage revision conflict",
            Self::Expired => "record expired",
            Self::NeedsLogin => "credential needs login",
            Self::NotFound => "record not found",
            Self::InvalidTransition => "invalid flow transition",
            Self::LeaseBusy => "credential refresh lease busy",
            Self::LeaseLost => "credential refresh lease lost",
            Self::ConnectionMismatch => "connection does not match",
            Self::InvalidRecord => "invalid credential record",
            Self::Storage => "credential storage operation failed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for Error {}

/// Result with redacted, typed errors.
pub type Result<T> = std::result::Result<T, Error>;
