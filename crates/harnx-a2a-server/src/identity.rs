//! Request ownership from proxy-supplied headers, not authentication.
//!
//! The proxy must strip client-supplied identity headers and set trusted values.
//! Only the resolved user ID may be persisted, never the raw request headers.

use a2a_lf::A2AError;
use a2a_server_lf::{jsonrpc::MAX_REQUEST_BODY_BYTES, middleware::ServiceParams};
use anyhow::Context;
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::web_url::first_value;

/// Implementation-defined JSON-RPC server error, outside A2A's -32001..-32009.
pub const MISSING_IDENTITY_CODE: i32 = -32000;

/// Session ownership. Anonymous mode is one shared principal, with no per-user isolation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Principal {
    Anonymous,
    User(String),
}

impl Principal {
    /// Stamp this value as `user_id` and compare it with the context binding owner.
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Self::Anonymous => None,
            Self::User(user_id) => Some(user_id),
        }
    }
}

/// Ordered identity header policy. Empty configuration enables shared anonymous mode.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    headers: Vec<HeaderName>,
}

impl Identity {
    /// Validate and normalize header names at startup, preserving CLI precedence.
    pub fn new(headers: &[String]) -> anyhow::Result<Self> {
        let headers = headers
            .iter()
            .map(|name| {
                HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid user-id-header name '{name}'"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { headers })
    }

    /// First configured header present wins. Empty or invalid values fail closed;
    /// neither later comma values nor lower-priority headers replace that identity.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Principal, A2AError> {
        if self.headers.is_empty() {
            return Ok(Principal::Anonymous);
        }
        self.headers
            .iter()
            .find(|name| headers.contains_key(*name))
            .and_then(|name| first_value(headers, name.as_str()))
            .map(|value| Principal::User(value.to_owned()))
            .ok_or_else(missing_identity)
    }

    /// RequestHandler receives headers through ServiceParams, not axum extensions.
    /// Keep this policy on the handler and resolve before accessing session state.
    pub fn resolve_service_params(&self, params: &ServiceParams) -> Result<Principal, A2AError> {
        let mut headers = HeaderMap::new();
        for name in &self.headers {
            if let Some(values) = params.get(name.as_str()) {
                let value = values
                    .first()
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(missing_identity)?;
                headers.insert(name.clone(), value);
                break;
            }
        }
        self.resolve(&headers)
    }
}

fn missing_identity() -> A2AError {
    A2AError::new(
        MISSING_IDENTITY_CODE,
        "missing or empty user identity header",
    )
}

/// Apply only to RPC routes, before upstream version checks and method dispatch.
pub(crate) async fn require_identity(
    State(identity): State<Identity>,
    mut request: Request,
    next: Next,
) -> Response {
    match identity.resolve(request.headers()) {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => {
            // Bound unauthenticated input too. An unreadable body has no recoverable id.
            let id = to_bytes(request.into_body(), MAX_REQUEST_BODY_BYTES)
                .await
                .ok()
                .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                .and_then(|body| body.get("id").cloned())
                .filter(|id| id.is_string() || id.is_number() || id.is_null())
                .unwrap_or(Value::Null);
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": error.code, "message": error.message},
                })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(names: &[&str]) -> Identity {
        harnx_core::require_nextest();
        Identity::new(&names.iter().map(|name| (*name).into()).collect::<Vec<_>>()).unwrap()
    }

    #[tokio::test]
    async fn identity_resolved_principal_is_available_as_request_extension() {
        use axum::{body::Body, routing::post, Extension, Router};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let app = Router::new()
            .route(
                "/",
                post(
                    |Extension(principal): Extension<Principal>, body: String| async move {
                        assert_eq!(body, "unchanged request body");
                        principal.user_id().unwrap_or("anonymous").to_owned()
                    },
                ),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                identity(&["x-user"]),
                require_identity,
            ));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("x-user", " user , ignored")
                    .body(Body::from("unchanged request body"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "user"
        );
    }

    #[tokio::test]
    async fn identity_unrecoverable_request_id_returns_null() {
        use axum::{body::Body, routing::post, Router};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        async fn unreachable_handler() -> StatusCode {
            panic!("unauthorized request dispatched")
        }
        let app = Router::new()
            .route("/", post(unreachable_handler))
            .route_layer(axum::middleware::from_fn_with_state(
                identity(&["x-user"]),
                require_identity,
            ));
        for body in [
            "not json",
            r#"{"id":{}}"#,
            r#"{"id":true}"#,
            r#"{"method":"GetTask"}"#,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["id"], Value::Null);
            assert_eq!(json["error"]["code"], MISSING_IDENTITY_CODE);
        }
    }

    #[test]
    fn identity_header_precedence() {
        let identity = identity(&["x-primary-user", "x-fallback-user"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-fallback-user", "fallback".parse().unwrap());
        assert_eq!(
            identity.resolve(&headers).unwrap().user_id(),
            Some("fallback")
        );
        headers.insert("x-primary-user", "primary".parse().unwrap());
        assert_eq!(
            identity.resolve(&headers).unwrap().user_id(),
            Some("primary")
        );
    }

    #[test]
    fn identity_comma_lists_and_repeated_headers_use_first_value() {
        let identity = identity(&["x-user"]);
        let mut headers = HeaderMap::new();
        headers.append("x-user", "  first-user  , second-user".parse().unwrap());
        headers.append("x-user", "third-user".parse().unwrap());
        assert_eq!(
            identity.resolve(&headers).unwrap().user_id(),
            Some("first-user")
        );
    }

    #[test]
    fn identity_empty_or_whitespace_first_value_is_rejected() {
        let identity = identity(&["x-user", "x-fallback"]);
        for value in ["", "  \t ", " , second-user", ",second-user"] {
            let mut headers = HeaderMap::new();
            headers.insert("x-user", value.parse().unwrap());
            headers.insert("x-fallback", "fallback".parse().unwrap());
            assert_eq!(
                identity.resolve(&headers).unwrap_err().code,
                MISSING_IDENTITY_CODE
            );
        }
    }

    #[test]
    fn identity_header_names_are_case_insensitive() {
        let identity = identity(&["X-UsEr-Id"]);
        let mut headers = HeaderMap::new();
        headers.insert("X-USER-ID", "user".parse().unwrap());
        assert_eq!(identity.resolve(&headers).unwrap().user_id(), Some("user"));
    }

    #[test]
    fn identity_missing_header_error() {
        let identity = identity(&["x-user"]);
        let error = identity.resolve(&HeaderMap::new()).unwrap_err();
        assert_eq!(error.code, MISSING_IDENTITY_CODE);
        assert_eq!(error.message, "missing or empty user identity header");
    }

    #[test]
    fn identity_anonymous_is_shared_even_with_user_headers() {
        let identity = identity(&[]);
        let mut headers = HeaderMap::new();
        headers.insert("x-user", "ignored".parse().unwrap());
        assert_eq!(identity.resolve(&headers).unwrap(), Principal::Anonymous);
        assert_eq!(identity.resolve(&HeaderMap::new()).unwrap().user_id(), None);
    }

    #[test]
    fn identity_invalid_header_name_fails_startup() {
        harnx_core::require_nextest();
        for name in ["", "bad header", "user:id", "user\n"] {
            assert!(Identity::new(&[name.into()]).is_err());
        }
    }

    #[test]
    fn identity_service_params_match_http_resolution() {
        let identity = identity(&["X-Primary", "X-Fallback"]);
        for entries in [
            vec![],
            vec![("x-fallback", vec!["fallback"])],
            vec![
                ("x-primary", vec![" first , second", "third"]),
                ("x-fallback", vec!["fallback"]),
            ],
            vec![
                ("x-primary", vec![" , second"]),
                ("x-fallback", vec!["fallback"]),
            ],
        ] {
            let mut headers = HeaderMap::new();
            let mut params = ServiceParams::new();
            for (name, values) in entries {
                for value in &values {
                    headers.append(HeaderName::from_static(name), value.parse().unwrap());
                }
                params.insert(name.into(), values.into_iter().map(str::to_owned).collect());
            }
            let http = identity.resolve(&headers).map_err(|error| error.code);
            let handler = identity
                .resolve_service_params(&params)
                .map_err(|error| error.code);
            assert_eq!(http, handler);
        }
    }
}
