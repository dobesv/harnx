//! Shared flow authorization and CSRF checks, before any route can claim a flow.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::Utc;
use harnx_credentials::{FlowStatus, PendingFlow, Principal, Versioned};

use super::pages::page;
use super::AppState;

/// The shared gate every route passes before doing anything (decision 7 step
/// 2/3): the flow exists, belongs to this server, is unexpired, and is still
/// `pending`. `403` for a trusted-header mismatch without any write.
pub(super) enum Gate {
    /// Checks passed; the flow is pending and unexpired.
    Pending(Box<Versioned<PendingFlow>>),
    /// Checks failed; the response to send instead.
    Refuse(Response),
}

/// What a route learned about the link it was asked to serve. The encrypted
/// flow store refuses to return an expired record at all, so expiry is a
/// separate case from "unknown link": both get an error page, different ones.
pub(super) enum LoadedFlow {
    Found(Box<Versioned<PendingFlow>>),
    Expired,
    Missing,
}

pub(super) fn gate(state: &AppState, headers: &HeaderMap, loaded: LoadedFlow) -> Gate {
    let flow = match loaded {
        LoadedFlow::Expired => {
            return Gate::Refuse(page(
                StatusCode::GONE,
                "Connection link expired",
                "This connection link has expired. Return to harnx and start the connection again.",
            ));
        }
        LoadedFlow::Missing => {
            return Gate::Refuse(page(
                StatusCode::NOT_FOUND,
                "Connection link not found",
                "This connection link is unknown here. Return to harnx and start the connection again.",
            ));
        }
        LoadedFlow::Found(flow) => flow,
    };
    if flow.record.server != state.lifecycle.identity_token {
        return Gate::Refuse(page(
            StatusCode::NOT_FOUND,
            "Connection link not found",
            "This connection link is unknown here. Return to harnx and start the connection again.",
        ));
    }
    // The store already refuses expired records, but keep the explicit check:
    // a record whose expiry passes between the read and this check still gets
    // the right page.
    if flow.record.expires_at <= Utc::now() {
        return Gate::Refuse(page(
            StatusCode::GONE,
            "Connection link expired",
            "This connection link has expired. Return to harnx and start the connection again.",
        ));
    }
    if flow.record.status != FlowStatus::Pending {
        return Gate::Refuse(page(
            StatusCode::CONFLICT,
            "Connection link already used",
            "This connection link has already been used or cancelled. Return to harnx and start the connection again.",
        ));
    }
    if let Some(refusal) = check_trusted_header(state, headers, &flow.record) {
        return Gate::Refuse(refusal);
    }
    Gate::Pending(flow)
}

/// A trusted-header mismatch is 403 with no writes (decision 7 step 2).
fn check_trusted_header(
    state: &AppState,
    headers: &HeaderMap,
    flow: &PendingFlow,
) -> Option<Response> {
    let owner_id = match &flow.owner {
        Principal::User(user_id) => user_id.as_str(),
        // `--single-user` can't be combined with a trusted header, so this
        // combination never serves a real flow.
        Principal::Ownerless => "",
    };
    if state
        .lifecycle
        .config
        .trusted_header_matches(headers, owner_id)
    {
        return None;
    }
    Some(page(
        StatusCode::FORBIDDEN,
        "Not allowed",
        "This connection link belongs to another user.",
    ))
}

/// Constant-time equality for the form's CSRF token against the flow's.
/// The length check leaks only the token length, which is not secret.
pub(super) fn csrf_matches(flow: &PendingFlow, presented: &str) -> bool {
    let expected = flow.csrf_token.expose().as_bytes();
    let presented = presented.as_bytes();
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .iter()
        .zip(presented)
        .map(|(a, b)| a ^ b)
        .fold(0u8, |acc, diff| acc | diff)
        == 0
}

pub(super) async fn load_flow(state: &AppState, challenge: &str) -> LoadedFlow {
    match state.lifecycle.flows.get_by_challenge(challenge).await {
        Ok(Some(flow)) => LoadedFlow::Found(Box::new(flow)),
        Ok(None) => LoadedFlow::Missing,
        Err(harnx_credentials::Error::Expired) => LoadedFlow::Expired,
        Err(_) => LoadedFlow::Missing,
    }
}
