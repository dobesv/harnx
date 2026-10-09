//! A2A ownership adapters for shared request identity resolution, not authentication.
//!
//! The proxy must strip client-supplied identity sources and set trusted values.
//! Only the resolved user ID may be persisted, never raw request headers or cookies.

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
use harnx_runtime::identity::{IdentitySource, IdentitySources, MembershipHeaders};
use serde_json::{json, Value};

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

/// Owned request identity. Memberships never become session ownership or metadata.
#[derive(Clone, Debug)]
pub struct RequestIdentity {
    pub principal: Principal,
    pub groups: Vec<String>,
    pub roles: Vec<String>,
}

impl From<Principal> for RequestIdentity {
    fn from(principal: Principal) -> Self {
        Self {
            principal,
            groups: Vec::new(),
            roles: Vec::new(),
        }
    }
}

/// Storage for slices borrowed by the core caller view.
pub struct RequestCaller<'a> {
    user: Option<&'a str>,
    groups: Vec<&'a str>,
    roles: Vec<&'a str>,
}

impl RequestCaller<'_> {
    pub fn view(&self) -> harnx_core::access_rules::CallerView<'_> {
        harnx_core::access_rules::CallerView {
            users: self.user.as_slice(),
            groups: &self.groups,
            roles: &self.roles,
        }
    }
}

impl RequestIdentity {
    pub fn caller(&self) -> RequestCaller<'_> {
        RequestCaller {
            user: self.principal.user_id(),
            groups: self.groups.iter().map(String::as_str).collect(),
            roles: self.roles.iter().map(String::as_str).collect(),
        }
    }
}

/// Ordered identity policy. Empty configuration enables shared anonymous mode.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    sources: IdentitySources,
    groups: MembershipHeaders,
    roles: MembershipHeaders,
    membership_names: Vec<HeaderName>,
}

impl Identity {
    /// Validate header/cookie sources at startup, preserving CLI precedence.
    pub fn new(sources: &[String]) -> anyhow::Result<Self> {
        Self::with_memberships(sources, &[], &[])
    }

    /// Membership sources are raw header names, not user sources or aliases.
    pub fn with_memberships(
        sources: &[String],
        groups: &[String],
        roles: &[String],
    ) -> anyhow::Result<Self> {
        let group_headers = MembershipHeaders::new(groups).context("invalid group-header names")?;
        let role_headers = MembershipHeaders::new(roles).context("invalid role-header names")?;
        Ok(Self {
            sources: IdentitySources::new(sources).context("invalid user-id-header sources")?,
            groups: group_headers,
            roles: role_headers,
            membership_names: groups
                .iter()
                .chain(roles)
                .map(|name| name.parse())
                .collect::<Result<_, _>>()?,
        })
    }

    pub(crate) fn validate_access_rules(&self, enabled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !enabled || !self.sources.sources().is_empty(),
            "access rules require a trusted identity source; configure --user-id-header"
        );
        Ok(())
    }

    /// First configured source present wins. Empty or invalid values fail closed.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Principal, A2AError> {
        if self.sources.sources().is_empty() {
            return Ok(Principal::Anonymous);
        }
        self.sources
            .resolve(headers)
            .map_err(|_| missing_identity())?
            .map(Principal::User)
            .ok_or_else(missing_identity)
    }

    pub fn resolve_request(&self, headers: &HeaderMap) -> Result<RequestIdentity, A2AError> {
        Ok(RequestIdentity {
            principal: self.resolve(headers)?,
            groups: self
                .groups
                .resolve(headers)
                .map_err(|_| missing_identity())?,
            roles: self
                .roles
                .resolve(headers)
                .map_err(|_| missing_identity())?,
        })
    }

    /// HTTP middleware validates raw values before the SDK drops non-text fields.
    /// Direct handler calls still resolve every configured membership occurrence.
    ///
    /// **Gotcha**: The `a2a-server-lf` SDK middleware silently drops headers whose values
    /// contain non-ASCII bytes, before handlers receive ServiceParams. Membership parsing must
    /// therefore run against the raw HeaderMap in middleware (`resolve_request`), not against
    /// ServiceParams. Reordering or skipping this step would allow malicious non-UTF-8 values
    /// to bypass validation and be silently dropped, resulting in unexpected empty memberships.
    pub fn resolve_request_params(
        &self,
        params: &ServiceParams,
    ) -> Result<RequestIdentity, A2AError> {
        let mut headers = HeaderMap::new();
        for name in &self.membership_names {
            Self::add_membership_params(name, params, &mut headers)?;
        }
        Ok(RequestIdentity {
            principal: self.resolve_service_params(params)?,
            groups: self
                .groups
                .resolve(&headers)
                .map_err(|_| missing_identity())?,
            roles: self
                .roles
                .resolve(&headers)
                .map_err(|_| missing_identity())?,
        })
    }

    fn add_membership_params(
        name: &HeaderName,
        params: &ServiceParams,
        headers: &mut HeaderMap,
    ) -> Result<(), A2AError> {
        if headers.contains_key(name) {
            return Ok(());
        }
        for value in params.get(name.as_str()).into_iter().flatten() {
            headers.append(name.clone(), value.parse().map_err(|_| missing_identity())?);
        }
        Ok(())
    }

    /// RequestHandler receives headers through ServiceParams, not axum extensions.
    /// Keep this policy on the handler and resolve before accessing session state.
    pub fn resolve_service_params(&self, params: &ServiceParams) -> Result<Principal, A2AError> {
        let mut headers = HeaderMap::new();
        for source in self.sources.sources() {
            Self::add_params_to_headers(source, params, &mut headers)?;
            // Cookie sources share fields, but each source must be resolved in order.
            if let Some(user) = source.resolve(&headers).map_err(|_| missing_identity())? {
                return Ok(Principal::User(user.to_owned()));
            }
        }
        self.resolve(&headers)
    }

    /// Add values from params to headers for the given source, if not already present.
    /// Returns `Err` if values exist but are empty.
    fn add_params_to_headers(
        source: &IdentitySource,
        params: &ServiceParams,
        headers: &mut HeaderMap,
    ) -> Result<(), A2AError> {
        let (name, limit) = match source {
            IdentitySource::Header(name) => (name.clone(), 1),
            IdentitySource::Cookie(_) => (HeaderName::from_static("cookie"), usize::MAX),
        };
        // Several cookie sources share the same header fields. Don't append them twice.
        if headers.contains_key(&name) {
            return Ok(());
        }
        let Some(values) = params.get(name.as_str()) else {
            return Ok(());
        };
        if values.is_empty() {
            return Err(missing_identity());
        }
        for value in values.iter().take(limit) {
            headers.append(name.clone(), value.parse().map_err(|_| missing_identity())?);
        }
        Ok(())
    }
}

pub(crate) fn missing_identity() -> A2AError {
    A2AError::new(
        MISSING_IDENTITY_CODE,
        "missing or empty user identity header",
    )
}

/// Resolve identity before protocol dispatch; protected cards use this layer too.
pub(crate) async fn require_identity(
    State(identity): State<Identity>,
    mut request: Request,
    next: Next,
) -> Response {
    match identity.resolve_request(request.headers()) {
        Ok(caller) => {
            request.extensions_mut().insert(caller.principal.clone());
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => rpc_error_response(request, StatusCode::UNAUTHORIZED, error).await,
    }
}

pub(crate) async fn rpc_error_response(
    request: Request,
    status: StatusCode,
    error: A2AError,
) -> Response {
    // Bound rejected input too. An unreadable body has no recoverable id.
    let id = to_bytes(request.into_body(), MAX_REQUEST_BODY_BYTES)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("id").cloned())
        .filter(|id| id.is_string() || id.is_number() || id.is_null())
        .unwrap_or(Value::Null);
    let error = if status == StatusCode::UNAUTHORIZED {
        // Preserve the existing identity error envelope when rules are off.
        json!({"code": error.code, "message": error.message})
    } else {
        json!(error.to_jsonrpc_error())
    };
    (
        status,
        Json(json!({"jsonrpc": "2.0", "id": id, "error": error})),
    )
        .into_response()
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
            assert_eq!(
                Identity::new(&[name.into()]).unwrap_err().to_string(),
                "invalid user-id-header sources"
            );
        }
    }

    #[test]
    fn identity_cookie_service_params_preserve_source_and_field_precedence() {
        for sources in [
            vec!["cookie:primary", "x-user", "cookie:fallback"],
            vec!["cookie:primary", "cookie:fallback", "x-user"],
            vec!["x-user", "cookie:primary", "cookie:fallback"],
        ] {
            let identity = identity(&sources);
            for cookies in [
                vec!["fallback=cookie-fallback", "primary=cookie-primary"],
                vec!["fallback=cookie-fallback"],
                vec!["primary=", "primary=later-primary"],
                vec!["primary=bad value"],
                vec!["unrelated=value"],
            ] {
                let mut headers = HeaderMap::new();
                let mut params = ServiceParams::new();
                headers.insert("x-user", "header-user".parse().unwrap());
                params.insert("x-user".into(), vec!["header-user".into()]);
                for cookie in &cookies {
                    headers.append("cookie", cookie.parse().unwrap());
                }
                params.insert(
                    "cookie".into(),
                    cookies.into_iter().map(str::to_owned).collect(),
                );
                let http = identity.resolve(&headers).map_err(|error| error.code);
                let handler = identity
                    .resolve_service_params(&params)
                    .map_err(|error| error.code);
                assert_eq!(http, handler, "{sources:?}");
            }
        }
    }

    #[test]
    fn identity_service_params_ignore_unusable_lower_priority_headers() {
        let identity = identity(&["x-primary", "x-fallback"]);
        let mut params = ServiceParams::new();
        params.insert(
            "x-primary".into(),
            vec!["primary".into(), "invalid\n".into()],
        );
        params.insert("x-fallback".into(), vec![]);
        assert_eq!(
            identity.resolve_service_params(&params).unwrap().user_id(),
            Some("primary")
        );
        for values in [vec![], vec!["invalid\n".into()], vec!["".into()]] {
            params.insert("x-primary".into(), values);
            params.insert("x-fallback".into(), vec!["fallback".into()]);
            assert_eq!(
                identity.resolve_service_params(&params).unwrap_err().code,
                MISSING_IDENTITY_CODE
            );
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

#[cfg(test)]
mod membership_tests;
