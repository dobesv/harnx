use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ClientRegistration, Principal, Secret};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    Pending,
    Claimed,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlowOutcome {
    Connected { connection_id: String },
    Failed { error_message: Secret<String> },
}

/// Discovery result and authorization URL fixed before handing the link to the user.
/// A callback on another instance must not rediscover different endpoints or state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthContext {
    pub metadata: Secret<rmcp::transport::auth::AuthorizationMetadata>,
    pub resource: String,
    pub authorization_url: Secret<String>,
}

/// Durable browser-flow state. Every secret stays inside the encrypted flow record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingFlow {
    pub owner: Principal,
    pub server: String,
    pub challenge_id: Secret<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: FlowStatus,
    pub outcome: Option<FlowOutcome>,
    pub csrf_token: Secret<String>,
    pub oauth_state: Secret<String>,
    pub pkce_verifier: Option<Secret<String>>,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub oauth_client: Option<ClientRegistration>,
    #[serde(default)]
    pub oauth_context: Option<OAuthContext>,
    /// rmcp issuer-validation and requested-scope state, persisted without reconstruction.
    pub authorization_state: Option<Secret<rmcp::transport::auth::StoredAuthorizationState>>,
}

/// Prepare OAuth/DCR before creating the link. Token-only flows can use defaults.
#[derive(Clone, Debug, Default)]
pub struct FlowOptions {
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub oauth_client: Option<ClientRegistration>,
    pub oauth_context: Option<OAuthContext>,
    /// When rmcp prepared PKCE, its CSRF value becomes the flow's OAuth state.
    pub authorization_state: Option<Secret<rmcp::transport::auth::StoredAuthorizationState>>,
}

impl PendingFlow {
    pub(super) fn validate_authorization_state(
        &self,
        state: &rmcp::transport::auth::StoredAuthorizationState,
    ) -> crate::Result<()> {
        if self.status != FlowStatus::Pending {
            return Err(crate::Error::InvalidTransition);
        }
        // The link's PKCE and issuer expectations are fixed before create returns.
        let original = self
            .authorization_state
            .as_ref()
            .ok_or(crate::Error::InvalidRecord)?;
        let original =
            serde_json::to_value(original.expose()).map_err(|_| crate::Error::InvalidRecord)?;
        let incoming = serde_json::to_value(state).map_err(|_| crate::Error::InvalidRecord)?;
        if state.csrf_token != *self.oauth_state.expose() || original != incoming {
            return Err(crate::Error::AuthenticationFailed);
        }
        Ok(())
    }
}
