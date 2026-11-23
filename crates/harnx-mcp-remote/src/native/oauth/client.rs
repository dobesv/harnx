//! OAuth client configuration and capture adapters.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use harnx_credentials::{ClientRegistration, Secret};
use oauth2::TokenResponse;
use rmcp::transport::auth::{
    self, AuthError, AuthorizationManager, AuthorizationMetadata, OAuthClientConfig,
    StoredAuthorizationState,
};

use super::{OAuthHttp, OAUTH_ERROR};

pub(crate) fn selected_auth_method(metadata: &AuthorizationMetadata, secret: bool) -> String {
    if !secret {
        return "none".into();
    }
    let methods = metadata
        .additional_fields
        .get("token_endpoint_auth_methods_supported")
        .and_then(serde_json::Value::as_array);
    if methods.is_some_and(|methods| {
        methods.iter().any(|method| method == "client_secret_post")
            && !methods.iter().any(|method| method == "client_secret_basic")
    }) {
        "client_secret_post".into()
    } else {
        "client_secret_basic".into()
    }
}

pub(crate) fn pin_auth_method(
    metadata: &mut AuthorizationMetadata,
    client: &ClientRegistration,
) -> Result<()> {
    anyhow::ensure!(
        matches!(
            client.token_endpoint_auth_method.as_str(),
            "none" | "client_secret_basic" | "client_secret_post"
        ),
        "unsupported OAuth token endpoint authentication method"
    );
    metadata.additional_fields.insert(
        "token_endpoint_auth_methods_supported".into(),
        serde_json::json!([client.token_endpoint_auth_method]),
    );
    Ok(())
}

pub(crate) fn client_config_from(
    client: &ClientRegistration,
    scopes: &[String],
) -> OAuthClientConfig {
    let mut config = OAuthClientConfig::new(&client.client_id, &client.redirect_uri)
        .with_scopes(scopes.to_vec());
    if let Some(secret) = &client.client_secret {
        config = config.with_client_secret(secret.expose());
    }
    config
}

pub(crate) async fn restored_manager(
    resource: &str,
    mut metadata: AuthorizationMetadata,
    client: &ClientRegistration,
    scopes: &[String],
    http: Arc<OAuthHttp>,
) -> Result<AuthorizationManager> {
    pin_auth_method(&mut metadata, client)?;
    let mut manager = AuthorizationManager::new_with_oauth_http_client(resource, http)
        .await
        .map_err(|_| anyhow!(OAUTH_ERROR))?;
    manager.set_metadata(metadata);
    manager
        .configure_client(client_config_from(client, scopes))
        .map_err(|_| anyhow!(OAUTH_ERROR))?;
    Ok(manager)
}

#[derive(Clone, Default)]
pub(crate) struct CaptureState(Arc<Mutex<Option<StoredAuthorizationState>>>);

impl CaptureState {
    pub(crate) fn take_state(&self) -> Result<StoredAuthorizationState> {
        self.0
            .lock()
            .map_err(|_| anyhow!(OAUTH_ERROR))?
            .take()
            .ok_or_else(|| anyhow!(OAUTH_ERROR))
    }
}

#[async_trait]
impl auth::StateStore for CaptureState {
    async fn save(&self, _csrf: &str, state: StoredAuthorizationState) -> Result<(), AuthError> {
        *self
            .0
            .lock()
            .map_err(|_| AuthError::CredentialStoreError("OAuth state unavailable".into()))? =
            Some(state);
        Ok(())
    }
    async fn load(&self, csrf: &str) -> Result<Option<StoredAuthorizationState>, AuthError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| AuthError::CredentialStoreError("OAuth state unavailable".into()))?
            .clone()
            .filter(|state| state.csrf_token == csrf))
    }
    async fn delete(&self, _csrf: &str) -> Result<(), AuthError> {
        self.0
            .lock()
            .map_err(|_| AuthError::CredentialStoreError("OAuth state unavailable".into()))?
            .take();
        Ok(())
    }
}

pub(crate) fn sanitize_account_label(
    label: Option<String>,
    code: &str,
    flow: &harnx_credentials::PendingFlow,
    token: &rmcp::transport::auth::OAuthTokenResponse,
    client: &ClientRegistration,
) -> Option<String> {
    label.filter(|l| {
        l.len() <= 200
            && !l.chars().any(char::is_control)
            && !l.contains(code)
            && !l.contains(flow.oauth_state.expose())
            && !l.contains(flow.csrf_token.expose())
            && !l.contains(token.access_token().secret())
            && !flow
                .pkce_verifier
                .as_ref()
                .is_some_and(|verifier| l.contains(verifier.expose()))
            && !token
                .refresh_token()
                .is_some_and(|t| l.contains(t.secret()))
            && !client
                .client_secret
                .as_ref()
                .is_some_and(|s| l.contains(s.expose()))
    })
}

pub(crate) fn build_oauth_material(
    context: &harnx_credentials::flow::OAuthContext,
    client: &ClientRegistration,
    token: &rmcp::transport::auth::OAuthTokenResponse,
    default_scopes: &[String],
) -> Result<harnx_credentials::OAuthMaterial> {
    let scopes = token
        .scopes()
        .map(|s| s.iter().map(|scope| scope.as_str().to_owned()).collect())
        .unwrap_or_else(|| default_scopes.to_vec());
    let access_expires_at = token
        .expires_in()
        .and_then(|d| chrono::TimeDelta::from_std(d).ok())
        .and_then(|d| chrono::Utc::now().checked_add_signed(d));
    let refresh_token = token
        .refresh_token()
        .map(|t| Secret::from(t.secret().as_str()));
    let extra_token_fields = Secret::new(token.extra_fields().0.clone().into_iter().collect());
    Ok(harnx_credentials::OAuthMaterial {
        issuer: context
            .metadata
            .expose()
            .issuer
            .clone()
            .ok_or_else(|| anyhow!(OAUTH_ERROR))?,
        resource: context.resource.clone(),
        authorization_metadata: Some(context.metadata.clone()),
        client: client.clone(),
        scopes,
        access_token: Secret::from(token.access_token().secret().as_str()),
        access_expires_at,
        refresh_token,
        extra_token_fields,
    })
}
