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
    // Authorization-only snapshot; session creation keeps the raw user_id.
    pub(super) expanded_users: Option<Vec<String>>,
}

/// Storage for the slices borrowed by the core caller view.
pub(super) struct RequestCaller<'a> {
    users: Vec<&'a str>,
    groups: Vec<&'a str>,
    roles: Vec<&'a str>,
}

impl RequestCaller<'_> {
    pub(super) fn view(&self) -> CallerView<'_> {
        CallerView {
            users: &self.users,
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

    /// Borrow authorization users and unchanged memberships from the request snapshot.
    pub(super) fn caller(&self) -> RequestCaller<'_> {
        RequestCaller {
            users: match &self.expanded_users {
                Some(users) => users.iter().map(String::as_str).collect(),
                None => self.user_id.iter().map(String::as_str).collect(),
            },
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
        let user_id = self.identity_sources.resolve(req.headers())?;
        let groups = self.group_headers.resolve(req.headers())?;
        let roles = self.role_headers.resolve(req.headers())?;

        let expanded_users = match (&user_id, self.user_aliases.as_deref()) {
            (Some(user), Some(aliases)) => Some(aliases.expand_caller(user).as_slice().to_vec()),
            _ => None,
        };

        Ok(RequestIdentity {
            user_id,
            groups,
            roles,
            expanded_users,
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

#[cfg(test)]
#[path = "user_alias_tests.rs"]
mod user_alias_tests;
