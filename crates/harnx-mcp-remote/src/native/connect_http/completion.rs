//! Token submission and the claimed-flow credential/catalog publication tail.

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Form;
use chrono::{DateTime, Utc};
use harnx_credentials::{FlowStatus, NewCredential, PendingFlow, Secret, TokenMaterial, Versioned};
use serde::Deserialize;

use super::gate::{csrf_matches, gate, load_flow, Gate};
use super::pages::{escape_html, page, parse_expiry, sanitize_message};
use super::AppState;

#[derive(Deserialize)]
pub(super) struct TokenForm {
    csrf_token: String,
    token: String,
    expires_at: Option<String>,
}

/// The completing tail every finishing route shares (decision 7 step 3):
/// write the credential, then the catalog and registration fenced by the new
/// `connection_id`, and mark the flow completed. Any failure marks the flow
/// failed with a safe, user-facing message.
pub(super) async fn complete_with_credential(
    state: &AppState,
    challenge: &str,
    claimed: Versioned<PendingFlow>,
    input: NewCredential,
) -> Response {
    // Failure text can carry upstream-sourced JSON-RPC errors, and a
    // provider can echo the submitted credential in them. Redact every form
    // of the just-submitted secret before the message is stored, shown, or
    // repeated to the caller by the connect tool.
    let secrets = credential_secrets(&input);
    let lifecycle = &state.lifecycle;
    let owner = claimed.record.owner.clone();
    let credential = match lifecycle.connect_credential(&owner, input).await {
        Ok(credential) => credential,
        Err(error) => {
            return finish_failed(state, challenge, claimed, &format!("{error:#}"), &secrets).await;
        }
    };
    match lifecycle
        .finish_connection(&owner, challenge, &credential)
        .await
    {
        Ok(_) => page(
            StatusCode::OK,
            "Connected",
            "Connected. Return to harnx and click Continue.",
        ),
        Err(error) => {
            finish_failed(state, challenge, claimed, &format!("{error:#}"), &secrets).await
        }
    }
}

/// Every form the submitted secret can appear in: raw, `scheme token`, and
/// the `Bearer <token>` header an OAuth exchange produced.
fn credential_secrets(input: &NewCredential) -> Vec<String> {
    match &input.material {
        harnx_credentials::CredentialMaterial::Token(token) => {
            let mut secrets = vec![token.token.expose().to_string()];
            if let Some(scheme) = &token.scheme {
                secrets.push(format!("{} {}", scheme, token.token.expose()));
            }
            secrets
        }
        harnx_credentials::CredentialMaterial::Oauth2(material) => {
            let mut secrets = vec![material.access_token.expose().to_string()];
            secrets.push(format!("Bearer {}", material.access_token.expose()));
            secrets
        }
    }
}

/// Mark a claimed flow failed and render the error page. The stored message
/// must be safe for the tool result to repeat: known secrets are redacted
/// and the result is trimmed to a sane length.
pub(super) async fn finish_failed(
    state: &AppState,
    challenge: &str,
    claimed: Versioned<PendingFlow>,
    message: &str,
    secrets: &[String],
) -> Response {
    let safe = sanitize_message(&crate::native::secret_redaction::redact_secrets(
        message, secrets,
    ));
    // rmcp consumes authorization state through the shared flow store, which
    // advances its KV revision without changing this claim.
    let revision = state
        .lifecycle
        .flows
        .get_by_challenge(challenge)
        .await
        .ok()
        .flatten()
        .filter(|flow| flow.record.status == FlowStatus::Claimed)
        .map_or(claimed.revision, |flow| flow.revision);
    let _ = state
        .lifecycle
        .flows
        .fail(challenge, revision, safe.clone())
        .await;
    page(
        StatusCode::BAD_GATEWAY,
        "Connection failed",
        &escape_html(&format!(
            "{safe}. Return to harnx and call the connect tool again."
        )),
    )
}

pub(super) async fn token_complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    form: Result<Form<TokenForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let flow = match gate(&state, &headers, load_flow(&state, &id).await) {
        Gate::Pending(flow) => flow,
        Gate::Refuse(response) => return response,
    };
    if !state.lifecycle.config.auth_methods.token {
        return page(
            StatusCode::NOT_FOUND,
            "Token authentication unavailable",
            "Use the configured sign-in method.",
        );
    }
    let Ok(Form(form)) = form else {
        // Malformed input doesn't consume the flow; the page form works again.
        return page(
            StatusCode::BAD_REQUEST,
            "Invalid form",
            "The submitted form is invalid. Go back and try again.",
        );
    };
    if form.token.trim().is_empty() {
        return page(
            StatusCode::BAD_REQUEST,
            "Missing token",
            "Enter the token to connect. Go back and try again.",
        );
    }
    if !csrf_matches(&flow.record, &form.csrf_token) {
        return page(
            StatusCode::FORBIDDEN,
            "Not allowed",
            "Open this page again from your harnx connection link and try again.",
        );
    }
    let expires_at = match parse_expiry(form.expires_at.as_deref()) {
        Ok(expires_at) => expires_at,
        Err(message) => {
            return page(
                StatusCode::BAD_REQUEST,
                "Invalid expiry date",
                &escape_html(message),
            );
        }
    };
    // The claim CAS is the single gate to writing: the loser of a race or a
    // replay gets an error page and writes nothing.
    let claimed = match claim_flow(&state, &id, flow.revision).await {
        Ok(claimed) => claimed,
        Err(response) => return *response,
    };
    let input = token_credential(&state.lifecycle.config, form.token, expires_at);
    complete_with_credential(&state, &id, claimed, input).await
}

pub(super) async fn claim_flow(
    state: &AppState,
    challenge: &str,
    revision: u64,
) -> Result<Versioned<PendingFlow>, Box<Response>> {
    state.lifecycle.flows.claim(challenge, revision).await.map_err(|_| {
        Box::new(page(
            StatusCode::CONFLICT,
            "Connection link already used",
            "This connection link has already been used. Return to harnx and start the connection again.",
        ))
    })
}

fn token_credential(
    config: &crate::native::NativeConfig,
    token: String,
    expires_at: Option<DateTime<Utc>>,
) -> NewCredential {
    NewCredential {
        material: harnx_credentials::CredentialMaterial::Token(TokenMaterial {
            token: Secret::new(token),
            header: config.token_header.as_str().to_string(),
            scheme: config.token_scheme.clone(),
        }),
        account_label: None,
        expires_at,
    }
}
