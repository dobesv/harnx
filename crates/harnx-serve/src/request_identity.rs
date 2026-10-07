//! Resolve API request identity before dispatch and handle unauthenticated preflight.

use super::{ret_err, set_cors_header, AppResponse, Server};
use http::{Method, Response, StatusCode};
use hyper::body::Incoming;
use std::time::Instant;

/// Resolved once at the API boundary, before any route can create metadata.
#[derive(Clone, Default)]
pub(super) struct RequestUserId(pub(super) Option<String>);

impl RequestUserId {
    /// Identity `prepare_request` resolved for this request, if any.
    pub(super) fn of<B>(req: &hyper::Request<B>) -> Option<String> {
        req.extensions().get::<Self>().and_then(|id| id.0.clone())
    }
}

impl Server {
    pub(super) fn prepare_request(
        &self,
        req: &mut hyper::Request<Incoming>,
        started: Instant,
    ) -> Option<AppResponse> {
        // Preflight doesn't require identity; all API validation precedes dispatch.
        if req.method() == Method::OPTIONS {
            let mut response = Response::default();
            *response.status_mut() = StatusCode::NO_CONTENT;
            return Some(record_early_http_response(req, response, started));
        }
        if !req.uri().path().starts_with("/v1/") {
            return None;
        }
        match self.identity_sources.resolve(req.headers()) {
            Ok(user_id) => {
                req.extensions_mut().insert(RequestUserId(user_id));
                None
            }
            Err(error) => Some(record_early_http_response(
                req,
                identity_error_response(error),
                started,
            )),
        }
    }
}

fn record_early_http_response(
    req: &hyper::Request<Incoming>,
    mut response: AppResponse,
    started: Instant,
) -> AppResponse {
    set_cors_header(&mut response);
    harnx_metrics::record_http_request(
        req.method().as_str(),
        "other",
        response.status().as_u16(),
        started.elapsed(),
    );
    response
}

fn identity_error_response(error: harnx_runtime::identity::IdentityError) -> AppResponse {
    let mut response = ret_err(error);
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}
