use chrono::{DateTime, TimeDelta, Utc};
use oauth2::{AccessToken, RefreshToken, Scope, TokenResponse};
use rmcp::transport::auth::{OAuthTokenResponse, StoredCredentials};

use crate::{CredentialMaterial, CredentialRecord, Error, OAuthMaterial, Result, Secret};

/// Convert without losing full client registration from the authoritative record.
pub fn to_rmcp_credentials(record: &CredentialRecord) -> Result<StoredCredentials> {
    record.ensure_active()?;
    let CredentialMaterial::Oauth2(material) = &record.material else {
        return Err(Error::InvalidRecord);
    };
    let now = Utc::now();
    let mut response = token_response(material)?;
    if let Some(expires_at) = material.access_expires_at {
        response.set_expires_in(Some(&(expires_at - now).to_std().unwrap_or_default()));
    }
    Ok(StoredCredentials::new(
        material.client.client_id.clone(),
        Some(response),
        material.scopes.clone(),
        Some(now.timestamp().max(0) as u64),
    )
    .with_issuer(Some(material.issuer.clone())))
}

fn token_response(material: &OAuthMaterial) -> Result<OAuthTokenResponse> {
    let extra = serde_json::Value::Object(material.extra_token_fields.expose().clone());
    let extra = serde_json::from_value(extra).map_err(|_| Error::InvalidRecord)?;
    let mut response = OAuthTokenResponse::new(
        AccessToken::new(material.access_token.expose().clone()),
        oauth2::basic::BasicTokenType::Bearer,
        extra,
    );
    response.set_refresh_token(
        material
            .refresh_token
            .as_ref()
            .map(|token| RefreshToken::new(token.expose().clone())),
    );
    response.set_scopes(Some(
        material.scopes.iter().cloned().map(Scope::new).collect(),
    ));
    Ok(response)
}

pub(super) fn from_rmcp_credentials(
    record: &CredentialRecord,
    credentials: StoredCredentials,
) -> Result<CredentialMaterial> {
    let previous = matching_oauth(record, &credentials)?;
    let response = credentials.token_response.ok_or(Error::InvalidRecord)?;
    let mut material = previous.clone();
    material.access_token = Secret::from(response.access_token().secret().clone());
    material.refresh_token = response
        .refresh_token()
        .map(|refresh| Secret::from(refresh.secret().clone()))
        .or_else(|| previous.refresh_token.clone());
    material.access_expires_at = access_expiry(&response, credentials.token_received_at)?;
    material.scopes = granted_scopes(credentials.granted_scopes, &previous.scopes);
    let extra = serde_json::to_value(response.extra_fields()).map_err(|_| Error::InvalidRecord)?;
    material.extra_token_fields =
        Secret::new(extra.as_object().ok_or(Error::InvalidRecord)?.clone());
    Ok(CredentialMaterial::Oauth2(Box::new(material)))
}

fn matching_oauth<'a>(
    record: &'a CredentialRecord,
    credentials: &StoredCredentials,
) -> Result<&'a OAuthMaterial> {
    let CredentialMaterial::Oauth2(previous) = &record.material else {
        return Err(Error::InvalidRecord);
    };
    if credentials.client_id != previous.client.client_id
        || credentials
            .issuer
            .as_ref()
            .is_some_and(|issuer| issuer != &previous.issuer)
    {
        return Err(Error::ConnectionMismatch);
    }
    Ok(previous)
}

fn access_expiry(
    response: &OAuthTokenResponse,
    received: Option<u64>,
) -> Result<Option<DateTime<Utc>>> {
    let received = received
        .map(|timestamp| i64::try_from(timestamp).map_err(|_| Error::InvalidRecord))
        .transpose()?
        .unwrap_or_else(|| Utc::now().timestamp());
    let received = DateTime::from_timestamp(received, 0).ok_or(Error::InvalidRecord)?;
    response
        .expires_in()
        .map(|ttl| {
            received
                .checked_add_signed(TimeDelta::from_std(ttl).map_err(|_| Error::InvalidRecord)?)
                .ok_or(Error::InvalidRecord)
        })
        .transpose()
}

fn granted_scopes(granted: Vec<String>, previous: &[String]) -> Vec<String> {
    if granted.is_empty() {
        previous.to_vec()
    } else {
        granted
    }
}
