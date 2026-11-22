//! Persisted credential schema and explicit secret wrappers.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{Error, Result};

const MAX_USER_ID_BYTES: usize = 512;

/// Serializable secret. Debug and Display never delegate to the wrapped value.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Explicitly access plaintext. Never log or send it through tool results.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

// qual:allow(dry, boilerplate) reason: "explicit secret formatting must never delegate to the wrapped value"
impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

// qual:allow(dry, boilerplate) reason: "keep secret conversions alongside explicit redacted formatting without a derive dependency"
impl From<String> for Secret<String> {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Secret<String> {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

/// Canonical session owner. Ownerless is never an alias for an identified user.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "user_id", rename_all = "snake_case")]
pub enum Principal {
    User(String),
    Ownerless,
}

impl Principal {
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Self::User(id) => Some(id),
            Self::Ownerless => None,
        }
    }

    /// `=` is legal in NATS keys but impossible in unpadded base64url.
    pub fn key_segment(&self) -> String {
        match self {
            Self::User(id) => URL_SAFE_NO_PAD.encode(id),
            Self::Ownerless => "=".into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Self::User(id) = self {
            if id.trim().is_empty() || id.len() > MAX_USER_ID_BYTES {
                return Err(Error::InvalidRecord);
            }
        }
        Ok(())
    }
}

/// Full client registration, including provider-specific DCR fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientRegistration {
    pub client_id: String,
    pub client_secret: Option<Secret<String>>,
    pub token_endpoint_auth_method: String,
    pub redirect_uri: String,
    /// Keep the complete DCR response. rmcp's StoredCredentials omits it.
    pub registration: Option<Secret<rmcp::transport::auth::ClientRegistrationResponse>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenMaterial {
    pub token: Secret<String>,
    pub header: String,
    pub scheme: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthMaterial {
    pub issuer: String,
    pub resource: String,
    /// Pin refresh endpoints to this connection's discovery result.
    #[serde(default)]
    pub authorization_metadata: Option<Secret<rmcp::transport::auth::AuthorizationMetadata>>,
    pub client: ClientRegistration,
    pub scopes: Vec<String>,
    pub access_token: Secret<String>,
    pub access_expires_at: Option<DateTime<Utc>>,
    pub refresh_token: Option<Secret<String>>,
    /// Preserve vendor token fields without exposing their Debug implementation.
    #[serde(default)]
    pub extra_token_fields: Secret<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialMaterial {
    Token(TokenMaterial),
    Oauth2(Box<OAuthMaterial>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Active,
    NeedsLogin,
}

/// Decrypted record. Use `CredentialStore::get`, not this type alone, to authorize.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CredentialRecord {
    pub schema_version: u32,
    pub owner_user_id: Option<String>,
    pub server: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: CredentialStatus,
    pub account_label: Option<String>,
    pub connection_id: String,
    pub material: CredentialMaterial,
}

impl CredentialRecord {
    /// Check absolute expiry and login status on every use.
    pub fn ensure_active(&self) -> Result<()> {
        if self.expires_at <= Utc::now() {
            return Err(Error::Expired);
        }
        if self.status != CredentialStatus::Active {
            return Err(Error::NeedsLogin);
        }
        Ok(())
    }
}

/// Connect input. `None` expiry uses the store's configured maximum (365 days by default).
#[derive(Clone, Debug)]
pub struct NewCredential {
    pub material: CredentialMaterial,
    pub account_label: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

pub(crate) fn server_segment(server: &str) -> String {
    URL_SAFE_NO_PAD.encode(server)
}

pub(crate) fn validate_server(server: &str) -> Result<()> {
    if server.trim().is_empty() {
        return Err(Error::InvalidRecord);
    }
    Ok(())
}
