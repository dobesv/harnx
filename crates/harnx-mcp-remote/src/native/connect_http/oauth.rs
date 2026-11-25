//! Provider redirects and authorization callbacks driven by encrypted pending state.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use harnx_credentials::{PendingFlow, Versioned};
use serde::Deserialize;

use super::completion::{claim_flow, complete_with_credential, finish_failed};
use super::gate::{gate, load_flow, Gate, LoadedFlow};
use super::pages::{escape_html, page};
use super::AppState;

#[derive(Deserialize)]
pub(super) struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    iss: Option<String>,
    error: Option<String>,
}

pub(super) async fn oauth_start(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let flow = match gate(&state, &headers, load_flow(&state, &id).await) {
        Gate::Pending(flow) => flow,
        Gate::Refuse(response) => return response,
    };
    let Some(oauth) = state.oauth.as_ref() else {
        return page(
            StatusCode::NOT_IMPLEMENTED,
            "OAuth is not available on this server",
            "This server is configured for token authentication. Use the token form instead.",
        );
    };
    // The redirect itself is T6's; claiming happens only at the callback.
    match oauth.redirect_url(&flow).await {
        Ok(url) => {
            let mut response = Redirect::temporary(url.as_str()).into_response();
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            response.headers_mut().insert(
                axum::http::header::REFERRER_POLICY,
                axum::http::HeaderValue::from_static("no-referrer"),
            );
            response
        }
        Err(message) => page(
            StatusCode::BAD_GATEWAY,
            "Cannot reach the sign-in provider",
            &escape_html(&format!("{message}.")),
        ),
    }
}

pub(super) async fn callback(
    State(state): State<AppState>,
    query: Result<Query<CallbackQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Ok(Query(query)) = query else {
        return page(
            StatusCode::BAD_REQUEST,
            "Invalid callback",
            "This callback is not valid.",
        );
    };
    let Some(presented_state) = query.state.as_deref() else {
        return page(
            StatusCode::BAD_REQUEST,
            "Missing state",
            "This callback is not valid.",
        );
    };
    let code = query.code.as_deref().unwrap_or_default();
    if code.is_empty() && query.error.is_none() {
        return page(
            StatusCode::BAD_REQUEST,
            "Missing authorization code",
            "This callback is not valid.",
        );
    }
    // `get_by_state` compares the presented state against the encrypted flow.
    let flow = state
        .lifecycle
        .flows
        .get_by_state(presented_state)
        .await
        .ok()
        .flatten();
    let Some(flow) = flow else {
        return page(
            StatusCode::NOT_FOUND,
            "Connection link not found",
            "This connection link is unknown here. Return to harnx and start the connection again.",
        );
    };
    match gate(&state, &headers, LoadedFlow::Found(Box::new(flow))) {
        Gate::Pending(flow) => complete_oauth(&state, &flow, &query).await,
        Gate::Refuse(response) => response,
    }
}

async fn complete_oauth(
    state: &AppState,
    flow: &Versioned<PendingFlow>,
    query: &CallbackQuery,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return page(
            StatusCode::NOT_IMPLEMENTED,
            "OAuth is not available on this server",
            "This server is configured for token authentication. Use the token form instead.",
        );
    };
    let challenge = flow.record.challenge_id.expose();
    // Claim before the exchange: an OAuth/token race must not redeem
    // a code (or rotate provider state) after another method won.
    let claimed = match claim_flow(state, challenge, flow.revision).await {
        Ok(claimed) => claimed,
        Err(response) => return *response,
    };
    if query.error.is_some() {
        return finish_failed(
            state,
            challenge,
            claimed,
            "OAuth authorization was declined; start a new connection",
            &[],
        )
        .await;
    }
    match oauth
        .exchange_code_with_issuer(
            &claimed,
            query.code.as_deref().unwrap_or_default(),
            query.iss.as_deref(),
        )
        .await
    {
        Ok(input) => complete_with_credential(state, challenge, claimed, input).await,
        Err(_) => {
            finish_failed(
                state,
                challenge,
                claimed,
                "OAuth code exchange failed; start a new connection",
                &[],
            )
            .await
        }
    }
}
