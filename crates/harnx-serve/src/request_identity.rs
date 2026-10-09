//! Resolve API request identity before dispatch and handle unauthenticated preflight.

use super::{ret_err, set_cors_header, AppResponse, Server};
use harnx_core::access_rules::CallerView;
use harnx_runtime::identity::IdentityError;
use http::{Method, Response, StatusCode};
use hyper::body::Incoming;
use std::time::Instant;

/// Owned request-local identity. Only `user_id` can become session metadata.
#[derive(Clone, Default)]
pub(super) struct RequestIdentity {
    pub(super) user_id: Option<String>,
    pub(super) groups: Vec<String>,
    pub(super) roles: Vec<String>,
}

/// Storage for the slices borrowed by the core caller view.
pub(super) struct RequestCaller<'a> {
    user: Option<&'a str>,
    groups: Vec<&'a str>,
    roles: Vec<&'a str>,
}

impl RequestCaller<'_> {
    pub(super) fn view(&self) -> CallerView<'_> {
        CallerView {
            users: self.user.as_slice(),
            groups: &self.groups,
            roles: &self.roles,
        }
    }
}

impl RequestIdentity {
    pub(super) fn of<B>(req: &hyper::Request<B>) -> Self {
        req.extensions().get::<Self>().cloned().unwrap_or_default()
    }

    pub(super) fn user_id<B>(req: &hyper::Request<B>) -> Option<String> {
        req.extensions()
            .get::<Self>()
            .and_then(|id| id.user_id.clone())
    }

    pub(super) fn caller(&self) -> RequestCaller<'_> {
        RequestCaller {
            user: self.user_id.as_deref(),
            groups: self.groups.iter().map(String::as_str).collect(),
            roles: self.roles.iter().map(String::as_str).collect(),
        }
    }
}

impl Server {
    /// Missing identity never disables checks or falls back to session defaults.
    pub(crate) fn access<B>(
        &self,
        req: &hyper::Request<B>,
    ) -> Option<(&harnx_core::access_rules::AccessRules, RequestIdentity)> {
        self.access_rules
            .as_deref()
            .map(|rules| (rules, RequestIdentity::of(req)))
    }

    fn resolve_identity<B>(
        &self,
        req: &hyper::Request<B>,
    ) -> Result<RequestIdentity, IdentityError> {
        Ok(RequestIdentity {
            user_id: self.identity_sources.resolve(req.headers())?,
            groups: self.group_headers.resolve(req.headers())?,
            roles: self.role_headers.resolve(req.headers())?,
        })
    }

    fn requires_user_identity(&self, path: &str) -> bool {
        let protected =
            path == "/v1/agents" || path.starts_with("/v1/agents/") || path.starts_with("/v1/cid/");
        self.access_rules.is_some() && protected
    }

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
        match self.resolve_identity(req) {
            Ok(identity) => {
                if self.requires_user_identity(req.uri().path()) && identity.user_id.is_none() {
                    return Some(record_early_http_response(
                        req,
                        identity_error_response("missing user identity"),
                        started,
                    ));
                }
                req.extensions_mut().insert(identity);
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

fn identity_error_response(error: impl std::fmt::Display) -> AppResponse {
    let mut response = ret_err(error);
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}
