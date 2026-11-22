use anyhow::{Context, Result};
use async_nats::jetstream;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{TimeDelta, Utc};
use harnx_credentials::{
    ClientRegistration, CredentialMaterial, CredentialStore, FlowOptions, Keyring, NewCredential,
    OAuthMaterial, Principal, Secret, TokenMaterial,
};
use rmcp::transport::auth::{ClientRegistrationResponse, StoredAuthorizationState};
use std::sync::Arc;

pub fn keyring() -> Result<Arc<Keyring>> {
    keyring_byte(42)
}

pub fn keyring_byte(byte: u8) -> Result<Arc<Keyring>> {
    let json =
        serde_json::json!({ "active": "test", "keys": { "test": STANDARD.encode([byte; 32]) } });
    Ok(Arc::new(Keyring::from_json(&serde_json::to_vec(&json)?)?))
}

pub async fn nats() -> Result<(harnx_test_bins::NatsServerHandle, jetstream::Context)> {
    let server = harnx_test_bins::spawn_nats_server()
        .await?
        .context("NATS tests require nats-server; set NATS_SERVER_BIN (no silent skips)")?;
    let client = async_nats::connect(server.url()).await?;
    Ok((server, jetstream::new(client)))
}

pub async fn credentials(js: &jetstream::Context) -> Result<CredentialStore> {
    Ok(CredentialStore::open(js, 1, keyring()?, "jira", Principal::User("alice".into())).await?)
}

pub fn token(value: &str) -> CredentialMaterial {
    CredentialMaterial::Token(TokenMaterial {
        token: Secret::from(value),
        header: "Authorization".into(),
        scheme: Some("Bearer".into()),
    })
}

pub fn connect(value: &str) -> NewCredential {
    NewCredential {
        material: token(value),
        account_label: Some("alice@example.test".into()),
        expires_at: None,
    }
}

pub fn client_registration() -> ClientRegistration {
    let mut full =
        ClientRegistrationResponse::new("client-123", vec!["https://example.test/callback".into()]);
    full.client_secret = Some("client-secret-marker".into());
    full.additional_fields.insert(
        "registration_access_token".into(),
        serde_json::json!("registration-secret-marker"),
    );
    ClientRegistration {
        client_id: "client-123".into(),
        client_secret: Some(Secret::from("client-secret-marker")),
        token_endpoint_auth_method: "client_secret_post".into(),
        redirect_uri: "https://example.test/callback".into(),
        registration: Some(Secret::new(full)),
    }
}

pub fn oauth(access: &str, refresh: Option<&str>) -> CredentialMaterial {
    CredentialMaterial::Oauth2(Box::new(OAuthMaterial {
        authorization_metadata: None,
        issuer: "https://issuer.test".into(),
        resource: "https://resource.test/mcp".into(),
        client: client_registration(),
        scopes: vec!["offline_access".into()],
        access_token: Secret::from(access),
        access_expires_at: Some(Utc::now() + TimeDelta::hours(1)),
        refresh_token: refresh.map(Secret::from),
        extra_token_fields: Secret::new(serde_json::Map::from_iter([(
            "vendor_secret".into(),
            serde_json::json!("vendor-secret-marker"),
        )])),
    }))
}

pub fn authorization_state() -> StoredAuthorizationState {
    StoredAuthorizationState::new_with_expected_issuer(
        &oauth2::PkceCodeVerifier::new("pkce-secret-marker".into()),
        &oauth2::CsrfToken::new_random(),
        Some("https://issuer.test".into()),
        true,
    )
    .with_requested_scopes(vec!["offline_access".into()])
}

pub fn flow_options() -> FlowOptions {
    FlowOptions {
        oauth_context: None,
        redirect_uri: "https://example.test/callback".into(),
        scopes: vec!["offline_access".into()],
        oauth_client: Some(client_registration()),
        authorization_state: Some(Secret::new(authorization_state())),
    }
}

pub fn tool(name: &str) -> rmcp::model::Tool {
    rmcp::model::Tool::new(
        name.to_owned(),
        "test tool",
        Arc::new(serde_json::Map::new()),
    )
}

pub async fn oauth_connection(
    js: &jetstream::Context,
    access: &str,
    refresh: &str,
) -> Result<(
    CredentialStore,
    harnx_credentials::Versioned<harnx_credentials::CredentialRecord>,
)> {
    let store = credentials(js).await?;
    let record = store
        .put_connect(
            0,
            NewCredential {
                material: oauth(access, Some(refresh)),
                ..connect("unused")
            },
        )
        .await?;
    Ok((store, record))
}

pub fn one_winner<T: std::fmt::Debug>(
    a: harnx_credentials::Result<T>,
    b: harnx_credentials::Result<T>,
    expected_error: harnx_credentials::Error,
) -> T {
    assert_ne!(a.is_ok(), b.is_ok());
    let (winner, loser) = match a {
        Ok(winner) => (winner, b.unwrap_err()),
        Err(loser) => (b.unwrap(), loser),
    };
    assert_eq!(loser, expected_error);
    winner
}
