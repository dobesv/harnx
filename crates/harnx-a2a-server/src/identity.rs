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
use harnx_core::user_aliases::UserAliases;
use harnx_runtime::identity::{
    merge_memberships, BearerIdentity, IdentitySelection, IdentitySource, IdentitySources,
    MembershipHeaders,
};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::bearer_verify::BearerVerifier;

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
    // Authorization-only snapshot; ownership always uses the raw principal.
    expanded_users: Option<Vec<String>>,
}

impl From<Principal> for RequestIdentity {
    fn from(principal: Principal) -> Self {
        Self {
            principal,
            groups: Vec::new(),
            roles: Vec::new(),
            expanded_users: None,
        }
    }
}

/// Storage for slices borrowed by the core caller view.
pub struct RequestCaller<'a> {
    users: Vec<&'a str>,
    groups: Vec<&'a str>,
    roles: Vec<&'a str>,
}

impl RequestCaller<'_> {
    pub fn view(&self) -> harnx_core::access_rules::CallerView<'_> {
        harnx_core::access_rules::CallerView {
            users: &self.users,
            groups: &self.groups,
            roles: &self.roles,
        }
    }
}

impl RequestIdentity {
    pub fn caller(&self) -> RequestCaller<'_> {
        RequestCaller {
            users: match &self.expanded_users {
                Some(users) => users.iter().map(String::as_str).collect(),
                None => self.principal.user_id().into_iter().collect(),
            },
            groups: self.groups.iter().map(String::as_str).collect(),
            roles: self.roles.iter().map(String::as_str).collect(),
        }
    }

    /// The caller-principals snapshot for this request, as resolved at entry.
    pub fn user_principals(&self) -> harnx_core::user_principals::UserPrincipals {
        harnx_core::user_principals::from_resolved(
            self.principal.user_id(),
            self.expanded_users.as_deref(),
            &self.groups,
            &self.roles,
        )
    }

    /// The snapshot to record when this request creates a session: none for
    /// an anonymous caller, so the session resolves the legacy fallback.
    pub fn stampable_principals(&self) -> Option<harnx_core::user_principals::UserPrincipals> {
        let principals = self.user_principals();
        (!principals.is_anonymous()).then_some(principals)
    }
}

/// Ordered identity policy. Empty configuration enables shared anonymous mode.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    sources: IdentitySources,
    groups: MembershipHeaders,
    roles: MembershipHeaders,
    membership_names: Vec<HeaderName>,
    aliases: Option<Arc<UserAliases>>,
    // Present only when bearer verification is configured; the sync resolve
    // paths fail closed when it is set, see `with_bearer_verification`.
    verifier: Option<Arc<BearerVerifier>>,
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
            aliases: None,
            sources: IdentitySources::new(sources).context("invalid user-id-header sources")?,
            groups: group_headers,
            roles: role_headers,
            membership_names: groups
                .iter()
                .chain(roles)
                .map(|name| name.parse())
                .collect::<Result<_, _>>()?,
            verifier: None,
        })
    }

    /// Require verified bearer tokens for bearer identity sources. When set,
    /// bearer sources resolve only from tokens that verify against the JWKS
    /// (`iss`, `aud`, RS256 signature, time claims); a token that fails
    /// verification fails closed as 401 / -32000 and later sources are never
    /// tried. Header and cookie sources are unaffected.
    ///
    /// Invariant for every identity-resolution entry point: either verify the
    /// selected bearer token, or fail closed while a verifier is configured.
    /// The sync decode-only paths (`resolve`, `resolve_request`,
    /// `resolve_service_params`, `resolve_request_params`) return
    /// `missing_identity()` because they can't await a fetch. A new entry
    /// point must add the guard or verify, and
    /// `sync_decode_only_paths_fail_closed_when_verification_is_enabled`
    /// must cover it.
    pub fn with_bearer_verification(mut self, verifier: Arc<BearerVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    pub(crate) fn validate_bearer_verification(&self) -> anyhow::Result<()> {
        if self.verifier.is_some() {
            anyhow::ensure!(
                self.sources.has_bearer_source(),
                "bearer verification flags require a bearer identity source; \
                 configure --user-id-header forge-fit or bearer-claim:CLAIM"
            );
        }
        Ok(())
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
        // Bearer verification is async, so it lives in `resolve_verified`.
        // Decoding a token here would bypass it; fail closed instead.
        if self.verifier.is_some() {
            return Err(missing_identity());
        }
        self.sources
            .resolve(headers)
            .map_err(|_| missing_identity())?
            .map(Principal::User)
            .ok_or_else(missing_identity)
    }

    /// Verification-aware principal resolution for both request paths. With no
    /// verifier configured this is exactly the decode-only `resolve`.
    async fn resolve_verified(&self, headers: &HeaderMap) -> Result<Principal, A2AError> {
        if self.sources.sources().is_empty() {
            return Ok(Principal::Anonymous);
        }
        match self
            .sources
            .select(headers)
            .map_err(|_| missing_identity())?
        {
            Some(IdentitySelection::Resolved(user)) => Ok(Principal::User(user)),
            Some(IdentitySelection::Bearer(bearer)) => {
                Ok(Principal::User(self.resolve_bearer(&bearer).await?))
            }
            None => Err(missing_identity()),
        }
    }

    /// Resolve a selected bearer source: verify when a verifier is configured,
    /// decode only when none is. Never retries later sources.
    async fn resolve_bearer(&self, bearer: &BearerIdentity<'_>) -> Result<String, A2AError> {
        let user = match &self.verifier {
            Some(verifier) => {
                let claims = verifier
                    .verify(bearer.token())
                    .await
                    .map_err(bearer_rejected)?;
                bearer.resolve_claims(&claims)
            }
            None => bearer.resolve_decode_only(),
        };
        user.map_err(|_| missing_identity())
    }

    /// Share startup-only aliases without changing the principal used for ownership.
    pub fn with_user_aliases(mut self, aliases: Option<Arc<UserAliases>>) -> Self {
        self.aliases = aliases;
        self
    }

    /// Expand caller once and return users, groups, and roles.
    /// Avoids redundant `expand_caller` calls; returned membership values are cloned.
    fn expand_caller_all(
        &self,
        principal: &Principal,
    ) -> (Option<Vec<String>>, Vec<String>, Vec<String>) {
        match (principal.user_id(), self.aliases.as_deref()) {
            (Some(user), Some(aliases)) => {
                let expanded = aliases.expand_caller(user);
                let users = expanded.as_slice().to_vec();
                let groups = expanded.groups().to_vec();
                let roles = expanded.roles().to_vec();
                (Some(users), groups, roles)
            }
            _ => (None, Vec::new(), Vec::new()),
        }
    }

    pub fn resolve_request(&self, headers: &HeaderMap) -> Result<RequestIdentity, A2AError> {
        let principal = self.resolve(headers)?;
        self.resolve_with_memberships(principal, headers)
    }

    /// HTTP middleware path: verify bearer sources when verification is
    /// configured, then validate membership headers. See `resolve_verified`.
    pub async fn resolve_request_verified(
        &self,
        headers: &HeaderMap,
    ) -> Result<RequestIdentity, A2AError> {
        let principal = self.resolve_verified(headers).await?;
        self.resolve_with_memberships(principal, headers)
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
        let principal = self.resolve_service_params(params)?;
        self.resolve_with_memberships(principal, &headers)
    }

    /// Handler path: verify bearer sources when verification is configured.
    /// Same semantics as `resolve_request_verified`, so both ownership
    /// resolution paths agree for valid and invalid tokens.
    pub async fn resolve_request_params_verified(
        &self,
        params: &ServiceParams,
    ) -> Result<RequestIdentity, A2AError> {
        let mut headers = HeaderMap::new();
        for name in &self.membership_names {
            Self::add_membership_params(name, params, &mut headers)?;
        }
        let principal = self.resolve_service_params_verified(params).await?;
        self.resolve_with_memberships(principal, &headers)
    }

    fn resolve_with_memberships(
        &self,
        principal: Principal,
        headers: &HeaderMap,
    ) -> Result<RequestIdentity, A2AError> {
        // Reject malformed headers before alias expansion or merging configured memberships.
        let header_groups = self
            .groups
            .resolve(headers)
            .map_err(|_| missing_identity())?;
        let header_roles = self
            .roles
            .resolve(headers)
            .map_err(|_| missing_identity())?;
        // Expand caller once and merge configured memberships with header memberships.
        let (expanded_users, configured_groups, configured_roles) =
            self.expand_caller_all(&principal);
        Ok(RequestIdentity {
            expanded_users,
            principal,
            groups: merge_memberships(header_groups, &configured_groups),
            roles: merge_memberships(header_roles, &configured_roles),
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
        // Same fail-closed guard as `resolve`: the sync loop decodes bearer
        // tokens directly and must never bypass verification.
        if self.verifier.is_some() {
            return Err(missing_identity());
        }
        let mut headers = HeaderMap::new();
        for source in self.sources.sources() {
            Self::add_params_to_headers(source, params, &mut headers)?;
            // Cookie sources share fields, but each source must be resolved in order.
            if let Some(user) = source.resolve(&headers).map_err(|_| missing_identity())? {
                return Ok(Principal::User(user));
            }
        }
        self.resolve(&headers)
    }

    /// Handler-side bearer verification, lazily per source to preserve the
    /// malformed-later-param ordering of `resolve_service_params`.
    async fn resolve_service_params_verified(
        &self,
        params: &ServiceParams,
    ) -> Result<Principal, A2AError> {
        let mut headers = HeaderMap::new();
        for source in self.sources.sources() {
            Self::add_params_to_headers(source, params, &mut headers)?;
            // Cookie sources share fields, but each source must be selected in order.
            match source.select(&headers).map_err(|_| missing_identity())? {
                Some(IdentitySelection::Resolved(user)) => return Ok(Principal::User(user)),
                Some(IdentitySelection::Bearer(bearer)) => {
                    return Ok(Principal::User(self.resolve_bearer(&bearer).await?));
                }
                None => continue,
            }
        }
        if self.sources.sources().is_empty() {
            return Ok(Principal::Anonymous);
        }
        Err(missing_identity())
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
            IdentitySource::ForgeFit | IdentitySource::BearerClaim(_) => {
                (HeaderName::from_static("authorization"), 1)
            }
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

/// Bearer verification failures fail closed with a reason category only; the
/// token, its claims and the JWKS body never appear in errors or logs.
fn bearer_rejected(failure: crate::bearer_verify::VerifyFailure) -> A2AError {
    // Fetch-path JWKS faults are bounded by the refetch throttle; warn so an
    // outage is visible at default log levels. Per-token rejections stay at
    // debug. Reason categories only, never token content.
    if failure.is_jwks_fault() {
        tracing::warn!(reason = failure.category(), "bearer token rejected");
    } else {
        tracing::debug!(reason = failure.category(), "bearer token rejected");
    }
    A2AError::new(MISSING_IDENTITY_CODE, failure.category())
}

/// Resolve identity before protocol dispatch; protected cards use this layer too.
pub(crate) async fn require_identity(
    State(identity): State<Identity>,
    mut request: Request,
    next: Next,
) -> Response {
    // Bind before matching: the verification future borrows the headers, and a
    // match scrutinee temporary would keep that borrow across the arms.
    let resolved = identity.resolve_request_verified(request.headers()).await;
    match resolved {
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

    #[test]
    fn identity_bearer_service_params_match_http_and_preserve_order() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let payload = br#"{"principal":"account-id","context":{"cloudId":"site-id"}}"#;
        let valid = format!("bearer e30.{}.signature", URL_SAFE_NO_PAD.encode(payload));
        for (source, expected) in [
            ("forge-fit", "CN=account-id,OU=site-id,O=atlassian.com"),
            ("bearer-claim:principal", "account-id"),
        ] {
            let identity = identity(&[source, "x-fallback"]);
            for (authorization, expected) in [
                (None, Ok("fallback")),
                (Some("Basic credentials"), Ok("fallback")),
                (Some(valid.as_str()), Ok(expected)),
                (Some("Bearer invalid-token"), Err(MISSING_IDENTITY_CODE)),
                (Some("Bearer"), Err(MISSING_IDENTITY_CODE)),
            ] {
                let mut headers = HeaderMap::new();
                let mut params = ServiceParams::new();
                headers.insert("x-fallback", "fallback".parse().unwrap());
                params.insert("x-fallback".into(), vec!["fallback".into()]);
                if let Some(authorization) = authorization {
                    headers.append("authorization", authorization.parse().unwrap());
                    headers.append("authorization", "Bearer ignored-token".parse().unwrap());
                    params.insert(
                        "authorization".into(),
                        vec![authorization.into(), "Bearer ignored-token".into()],
                    );
                }
                let http = identity
                    .resolve_request(&headers)
                    .map(|caller| caller.principal)
                    .map_err(|error| error.code);
                let handler = identity
                    .resolve_request_params(&params)
                    .map(|caller| caller.principal)
                    .map_err(|error| error.code);
                assert_eq!(http, expected.map(|user| Principal::User(user.into())));
                assert_eq!(handler, http);
            }
        }
        let identity = identity(&["x-primary", "forge-fit"]);
        let mut params = ServiceParams::new();
        params.insert("x-primary".into(), vec!["primary".into()]);
        params.insert("authorization".into(), vec!["invalid\n".into()]);
        assert_eq!(
            identity
                .resolve_request_params(&params)
                .unwrap()
                .principal
                .user_id(),
            Some("primary")
        );
    }

    mod bearer_verification {
        use super::*;
        use crate::bearer_verify::test_util::{
            forge_claims, sign, verifier, JwksServer, TestKey, JWKS_PRIMARY, PRIMARY_KEY,
            SECONDARY_PEM,
        };

        const DN: &str =
            "CN=557058:account-id,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com";

        async fn verified_identity(sources: &[&str]) -> (Identity, JwksServer) {
            let server = JwksServer::start(JWKS_PRIMARY).await;
            let identity = identity(sources)
                .with_bearer_verification(std::sync::Arc::new(verifier(&server.url)));
            (identity, server)
        }

        fn valid_token() -> String {
            sign(&forge_claims("557058:account-id"), &PRIMARY_KEY)
        }

        // Signed with a key that isn't in the served JWKS.
        fn forged_token() -> String {
            sign(
                &forge_claims("557058:account-id"),
                &TestKey {
                    kid: "primary-2026-01",
                    pem: SECONDARY_PEM,
                },
            )
        }

        fn bearer_headers(token: &str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            headers
        }

        fn bearer_params(token: &str) -> ServiceParams {
            let mut params = ServiceParams::new();
            params.insert("authorization".into(), vec![format!("Bearer {token}")]);
            params
        }

        #[tokio::test]
        async fn verified_paths_agree_and_reject_invalid_tokens() {
            let (identity, _server) = verified_identity(&["forge-fit"]).await;
            let http = identity
                .resolve_request_verified(&bearer_headers(&valid_token()))
                .await
                .unwrap();
            let handler = identity
                .resolve_request_params_verified(&bearer_params(&valid_token()))
                .await
                .unwrap();
            assert_eq!(http.principal, Principal::User(DN.into()));
            assert_eq!(handler.principal, http.principal);

            let error = identity
                .resolve_request_verified(&bearer_headers(&forged_token()))
                .await
                .unwrap_err();
            assert_eq!(error.code, MISSING_IDENTITY_CODE);
            assert_eq!(error.message, "bad bearer token signature");
            assert!(!error.message.contains("Bearer"));
            assert_eq!(
                identity
                    .resolve_request_params_verified(&bearer_params(&forged_token()))
                    .await
                    .unwrap_err()
                    .code,
                MISSING_IDENTITY_CODE
            );
        }

        #[tokio::test]
        async fn verified_missing_authorization_falls_through() {
            let (identity, _server) = verified_identity(&["forge-fit", "x-fallback"]).await;
            let mut headers = HeaderMap::new();
            headers.insert("x-fallback", "fallback-user".parse().unwrap());
            assert_eq!(
                identity
                    .resolve_request_verified(&headers)
                    .await
                    .unwrap()
                    .principal
                    .user_id(),
                Some("fallback-user")
            );
            let mut params = ServiceParams::new();
            params.insert("x-fallback".into(), vec!["fallback-user".into()]);
            assert_eq!(
                identity
                    .resolve_request_params_verified(&params)
                    .await
                    .unwrap()
                    .principal
                    .user_id(),
                Some("fallback-user")
            );
        }

        #[tokio::test]
        async fn verified_precedence_follows_configured_order() {
            // Both resolution paths (HTTP headers and handler ServiceParams)
            // must follow the same precedence; check them in lockstep.
            fn service_params(entries: &[(&str, &str)]) -> ServiceParams {
                let mut params = ServiceParams::new();
                for (name, value) in entries {
                    params.insert((*name).into(), vec![(*value).into()]);
                }
                params
            }

            // A present earlier header wins, even with an invalid token present.
            let (identity, _server) = verified_identity(&["x-user", "forge-fit"]).await;
            let mut headers = bearer_headers(&forged_token());
            headers.insert("x-user", "header-user".parse().unwrap());
            let params = service_params(&[
                ("x-user", "header-user"),
                ("authorization", &format!("Bearer {}", forged_token())),
            ]);
            for resolved in [
                identity.resolve_request_verified(&headers).await.unwrap(),
                identity
                    .resolve_request_params_verified(&params)
                    .await
                    .unwrap(),
            ] {
                assert_eq!(resolved.principal.user_id(), Some("header-user"));
            }

            // An invalid token at the bearer source fails closed; later
            // sources are never tried.
            let (identity, _server) = verified_identity(&["forge-fit", "x-user"]).await;
            let mut headers = bearer_headers(&forged_token());
            headers.insert("x-user", "header-user".parse().unwrap());
            let params = service_params(&[
                ("authorization", &format!("Bearer {}", forged_token())),
                ("x-user", "header-user"),
            ]);
            for error in [
                identity
                    .resolve_request_verified(&headers)
                    .await
                    .unwrap_err(),
                identity
                    .resolve_request_params_verified(&params)
                    .await
                    .unwrap_err(),
            ] {
                assert_eq!(error.code, MISSING_IDENTITY_CODE);
            }

            // No Authorization header at the bearer source falls through.
            let (identity, _server) = verified_identity(&["forge-fit", "x-user"]).await;
            let mut headers = HeaderMap::new();
            headers.insert("x-user", "header-user".parse().unwrap());
            let params = service_params(&[("x-user", "header-user")]);
            for resolved in [
                identity.resolve_request_verified(&headers).await.unwrap(),
                identity
                    .resolve_request_params_verified(&params)
                    .await
                    .unwrap(),
            ] {
                assert_eq!(resolved.principal.user_id(), Some("header-user"));
            }
        }

        #[tokio::test]
        async fn sync_decode_only_paths_fail_closed_when_verification_is_enabled() {
            let (identity, _server) = verified_identity(&["forge-fit"]).await;
            let headers = bearer_headers(&valid_token());
            for error in [
                identity.resolve(&headers).unwrap_err(),
                identity.resolve_request(&headers).unwrap_err(),
            ] {
                assert_eq!(error.code, MISSING_IDENTITY_CODE);
            }
            let params = bearer_params(&valid_token());
            for error in [
                identity.resolve_service_params(&params).unwrap_err(),
                identity.resolve_request_params(&params).unwrap_err(),
            ] {
                assert_eq!(error.code, MISSING_IDENTITY_CODE);
            }
        }

        #[tokio::test]
        async fn unverified_identity_decodes_bearer_without_any_jwks_fetch() {
            // No verification flags: decode-only behavior, zero fetches.
            let server = JwksServer::start(JWKS_PRIMARY).await;
            let identity = identity(&["forge-fit"]);
            let resolved = identity
                .resolve_request_verified(&bearer_headers(&valid_token()))
                .await
                .unwrap();
            assert_eq!(resolved.principal, Principal::User(DN.into()));
            assert_eq!(server.fetches(), 0);
        }
    }
}

#[cfg(test)]
mod membership_tests;

#[cfg(test)]
mod alias_tests;
