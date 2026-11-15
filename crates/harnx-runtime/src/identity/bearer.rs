//! Bearer extraction and decode-only identity mapping. No authentication happens here.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http::{header::AUTHORIZATION, HeaderMap};
use serde_json::{Map, Value};

use super::{IdentityError, IdentitySource};

/// A selected bearer source, not yet decoded or verified.
///
/// Doesn't implement `Debug` to keep raw tokens out of diagnostics.
pub struct BearerIdentity<'a> {
    source: &'a IdentitySource,
    token: &'a str,
}

impl BearerIdentity<'_> {
    /// Raw token for the caller's verifier. Never persist or log this value.
    pub fn token(&self) -> &str {
        self.token
    }

    /// Map a claims object to identity without reading or decoding the raw token.
    /// Verification-enabled callers must supply only claims returned by their verifier.
    pub fn resolve_claims(&self, claims: &Map<String, Value>) -> Result<String, IdentityError> {
        match self.source {
            IdentitySource::ForgeFit => forge_identity(claims),
            IdentitySource::BearerClaim(name) => claim_identity(claims, name),
            // Construction is private and only bearer sources can be selected.
            _ => Err(IdentityError::InvalidValue),
        }
    }

    /// Decode-only resolution. Never call this on a verification-enabled path:
    /// decode the verified claims with a verifier and `resolve_claims` instead.
    pub fn resolve_decode_only(&self) -> Result<String, IdentityError> {
        self.decode()
    }

    pub(super) fn decode(&self) -> Result<String, IdentityError> {
        let claims = decode_claims(self.token)?;
        self.resolve_claims(&claims)
    }
}

pub(super) fn select<'a>(
    source: &'a IdentitySource,
    headers: &'a HeaderMap,
) -> Result<Option<BearerIdentity<'a>>, IdentityError> {
    let Some(header) = headers.get(AUTHORIZATION) else {
        return Ok(None);
    };
    let raw = header.as_bytes().trim_ascii();
    let boundary = raw
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(raw.len());
    if !raw[..boundary].eq_ignore_ascii_case(b"Bearer") {
        return Ok(None);
    }
    let token = std::str::from_utf8(raw[boundary..].trim_ascii())
        .map_err(|_| IdentityError::InvalidValue)?;
    if token.is_empty() {
        return Err(IdentityError::EmptyValue);
    }
    Ok(Some(BearerIdentity { source, token }))
}

fn decode_claims(token: &str) -> Result<Map<String, Value>, IdentityError> {
    let mut segments = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(IdentityError::InvalidValue);
    };
    if [header, payload, signature]
        .iter()
        .any(|part| part.is_empty())
    {
        return Err(IdentityError::InvalidValue);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| IdentityError::InvalidValue)?;
    // Deserialize directly to an object, rejecting scalar and array payloads too.
    serde_json::from_slice(&payload).map_err(|_| IdentityError::InvalidValue)
}

fn forge_identity(claims: &Map<String, Value>) -> Result<String, IdentityError> {
    let cloud_id = claims
        .get("context")
        .and_then(|context| context.get("cloudId"))
        .and_then(Value::as_str)
        .ok_or(IdentityError::InvalidValue)?;
    validate_dn_component(cloud_id)?;
    let principal = match claims.get("principal") {
        None => None,
        Some(Value::String(value)) => {
            validate_dn_component(value)?;
            (value != "unidentified").then_some(value)
        }
        Some(_) => return Err(IdentityError::InvalidValue),
    };
    let organization = format!("OU={cloud_id},O=atlassian.com");
    Ok(match principal {
        Some(principal) => format!("CN={principal},{organization}"),
        None => organization,
    })
}

fn validate_dn_component(value: &str) -> Result<(), IdentityError> {
    if value.is_empty() || value.trim() != value {
        return Err(IdentityError::InvalidValue);
    }
    if value.chars().any(char::is_control) {
        return Err(IdentityError::InvalidValue);
    }
    if value.contains(&[',', '=', '+', '<', '>', '#', ';', '"', '\\'][..]) {
        return Err(IdentityError::InvalidValue);
    }
    Ok(())
}

fn claim_identity(claims: &Map<String, Value>, name: &str) -> Result<String, IdentityError> {
    let value = claims
        .get(name)
        .and_then(Value::as_str)
        .ok_or(IdentityError::InvalidValue)?;
    if value.chars().any(char::is_control) {
        return Err(IdentityError::InvalidValue);
    }
    let value = value.trim();
    if value.is_empty() {
        return Err(IdentityError::EmptyValue);
    }
    Ok(value.to_owned())
}
