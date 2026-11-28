#![cfg(test)]
//! Shared test fixture and mock OAuth 2.0 / MCP server for native OAuth integration tests.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::Utc;
use harnx_credentials::catalog::CATALOGS_BUCKET;
use harnx_credentials::flow::FLOWS_BUCKET;
use harnx_credentials::store::CREDENTIALS_BUCKET;
pub use harnx_credentials::{CredentialMaterial, CredentialRecord, Principal};
use harnx_mcp_remote::native::registrations::Lifecycle;
use harnx_mcp_remote::native::toolset::RemoteCredentialsToolset;
use harnx_mcp_remote::native::{NativeConfig, NativeServer};
use harnx_toolset::{ToolInputRequired, ToolInvokeError};
use harnx_toolset_server::{HandleAudience, RegistrationHandle, StoredRegistration};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, DiscoverResult, ErrorData, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::native_common::*;

pub fn generate_secret(prefix: &str) -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    static COUNTER: AtomicUsize = AtomicUsize::new(1);
    let c = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}-{nanos}-{c}")
}

pub fn audience(user: &str) -> HandleAudience {
    HandleAudience::User(user.to_string())
}

pub fn alice_principal() -> Principal {
    Principal::User("alice".into())
}

pub fn connect_page_url(base: &str, challenge_id: &str) -> String {
    format!("{base}/connect/{challenge_id}")
}

pub fn collect_credential_secrets(record: &CredentialRecord, secrets: &mut Vec<String>) {
    if let CredentialMaterial::Oauth2(material) = &record.material {
        secrets.push(material.access_token.expose().clone());
        if let Some(refresh) = &material.refresh_token {
            secrets.push(refresh.expose().clone());
        }
    } else {
        panic!("expected OAuth credential");
    }
}

pub async fn shorten_access_expiry_to_near_expiry(lifecycle: &Lifecycle, principal: &Principal) {
    let store = lifecycle
        .credentials
        .for_principal(principal.clone())
        .expect("store");
    let lease = lifecycle
        .leases
        .acquire(&store)
        .await
        .expect("acquire lease");
    let snapshot = lease.credential().expect("snapshot");
    let mut mat = snapshot.record.material.clone();
    if let CredentialMaterial::Oauth2(ref mut oauth) = mat {
        oauth.access_expires_at = Some(Utc::now() + chrono::Duration::seconds(10));
    }
    lease.commit(mat).await.expect("commit shortened expiry");
}

#[derive(Clone)]
pub struct RecordedAuth {
    pub client_id: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub resource: Option<String>,
}

#[derive(Clone)]
pub struct RecordedToken {
    pub grant_type: Option<String>,
    pub code_verifier: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub resource: Option<String>,
}

pub struct MockOAuthServerState {
    pub base_url: String,
    pub dcr_client_id: String,
    pub dcr_client_secret: String,
    pub auth_code: String,
    pub initial_access_token: String,
    pub initial_refresh_token: String,
    pub initial_expires_in: u64,
    pub refresh_expires_in: AtomicU64,
    pub dcr_422: AtomicBool,
    pub reject_authorization: AtomicBool,
    pub invalid_grant: AtomicBool,
    pub transient_refresh_error: AtomicBool,
    pub simulate_403: AtomicBool,
    pub rejected_tokens: Mutex<HashSet<String>>,
    pub refresh_count: AtomicUsize,
    pub dcr_count: AtomicUsize,
    pub token_request_count: AtomicUsize,
    pub mcp_request_count: AtomicUsize,
    pub recorded_auths: Mutex<Vec<RecordedAuth>>,
    pub recorded_tokens: Mutex<Vec<RecordedToken>>,
    pub refresh_pause_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub refresh_resume_rx: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    pub valid_tokens: Mutex<HashSet<String>>,
    pub rotated_counter: AtomicUsize,
}

impl MockOAuthServerState {
    pub fn issue_rotated_tokens(&self) -> (String, String) {
        let idx = self.rotated_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let new_access = generate_secret(&format!("access-rot-{idx}"));
        let new_refresh = generate_secret(&format!("refresh-rot-{idx}"));
        self.valid_tokens.lock().unwrap().insert(new_access.clone());
        (new_access, new_refresh)
    }

    pub fn resolve_label(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        let auth = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|p| p.headers.get(axum::http::header::AUTHORIZATION))
            .and_then(|h| h.to_str().ok())?;
        let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
        if self.valid_tokens.lock().unwrap().contains(token) {
            Some("alice@example.test".into())
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct MockMcpHandler {
    pub state: Arc<MockOAuthServerState>,
}

#[tool_router]
impl MockMcpHandler {
    #[tool(description = "Report the connected user label")]
    async fn whoami(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.state.mcp_request_count.fetch_add(1, Ordering::SeqCst);
        let label = self.state.resolve_label(&context);
        if let Some(label) = label {
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "user:{label}"
            ))]))
        } else {
            Err(ErrorData::internal_error(
                "no known credential presented",
                None,
            ))
        }
    }

    #[tool(description = "A per-user search tool")]
    async fn search(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.state.mcp_request_count.fetch_add(1, Ordering::SeqCst);
        let label = self
            .state
            .resolve_label(&context)
            .unwrap_or_else(|| "unknown".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "search:{label}"
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for MockMcpHandler {
    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        ))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = vec![
            rmcp::model::Tool::new("whoami", "Report user", Arc::new(serde_json::Map::new())),
            rmcp::model::Tool::new("search", "Search", Arc::new(serde_json::Map::new())),
        ];
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        );
        info.server_info = rmcp::model::Implementation::new("mock-oauth-mcp", "1.0.0");
        info
    }
}

pub async fn oauth_protected_resource(State(state): State<Arc<MockOAuthServerState>>) -> Response {
    let body = json!({
        "resource": format!("{}/mcp", state.base_url),
        "authorization_servers": [state.base_url.clone()],
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

pub async fn oauth_as_metadata(State(state): State<Arc<MockOAuthServerState>>) -> Response {
    let body = json!({
        "issuer": state.base_url.clone(),
        "authorization_endpoint": format!("{}/oauth/authorize", state.base_url),
        "token_endpoint": format!("{}/oauth/token", state.base_url),
        "registration_endpoint": format!("{}/oauth/register", state.base_url),
        "userinfo_endpoint": format!("{}/oauth/userinfo", state.base_url),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic", "none"],
        "scopes_supported": ["offline_access", "read", "write"]
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

pub async fn oauth_register(
    State(state): State<Arc<MockOAuthServerState>>,
    _body: axum::body::Bytes,
) -> Response {
    state.dcr_count.fetch_add(1, Ordering::SeqCst);
    if state.dcr_422.load(Ordering::SeqCst) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "error": "invalid_client_metadata",
                "error_description": "dynamic registration disallowed"
            })
            .to_string(),
        )
            .into_response();
    }

    let res = json!({
        "client_id": state.dcr_client_id.clone(),
        "client_secret": state.dcr_client_secret.clone(),
        "token_endpoint_auth_method": "client_secret_post",
        "redirect_uris": [format!("{}/callback", state.base_url)]
    });
    (
        StatusCode::CREATED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        res.to_string(),
    )
        .into_response()
}

pub async fn oauth_authorize(
    State(state): State<Arc<MockOAuthServerState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    state.recorded_auths.lock().unwrap().push(RecordedAuth {
        client_id: params.get("client_id").cloned(),
        state: params.get("state").cloned(),
        code_challenge: params.get("code_challenge").cloned(),
        code_challenge_method: params.get("code_challenge_method").cloned(),
        resource: params.get("resource").cloned(),
    });

    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let oauth_state = params.get("state").cloned().unwrap_or_default();

    let mut target = match Url::parse(&redirect_uri) {
        Ok(url) => url,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid redirect_uri").into_response(),
    };

    if state.reject_authorization.load(Ordering::SeqCst) {
        target
            .query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", &oauth_state)
            .append_pair("iss", &state.base_url);
        return Redirect::temporary(target.as_str()).into_response();
    }

    target
        .query_pairs_mut()
        .append_pair("code", &state.auth_code)
        .append_pair("state", &oauth_state)
        .append_pair("iss", &state.base_url);
    Redirect::temporary(target.as_str()).into_response()
}

fn parse_basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let auth_val = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let b64 = auth_val.strip_prefix("Basic ")?;
    let decoded = STANDARD.decode(b64.trim()).ok()?;
    let cred_str = String::from_utf8(decoded).ok()?;
    let (id, sec) = cred_str.split_once(':')?;
    Some((id.to_string(), sec.to_string()))
}

async fn handle_refresh_token(state: &MockOAuthServerState) -> Response {
    state.refresh_count.fetch_add(1, Ordering::SeqCst);

    let pause_tx = state.refresh_pause_tx.lock().unwrap().take();
    if let Some(tx) = pause_tx {
        let _ = tx.send(());
        let resume_rx = state.refresh_resume_rx.lock().unwrap().take();
        if let Some(rx) = resume_rx {
            let _ = rx.await;
        }
    }

    if state.invalid_grant.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "error": "invalid_grant",
                "error_description": "refresh token revoked"
            })
            .to_string(),
        )
            .into_response();
    }

    if state.transient_refresh_error.swap(false, Ordering::SeqCst) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "error": "server_error",
                "error_description": "temporary upstream glitch"
            })
            .to_string(),
        )
            .into_response();
    }

    let (new_access, new_refresh) = state.issue_rotated_tokens();
    let res = json!({
        "access_token": new_access,
        "token_type": "Bearer",
        "expires_in": state.refresh_expires_in.load(Ordering::SeqCst),
        "refresh_token": new_refresh,
        "scope": "offline_access"
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        res.to_string(),
    )
        .into_response()
}

pub async fn oauth_token(
    State(state): State<Arc<MockOAuthServerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    state.token_request_count.fetch_add(1, Ordering::SeqCst);
    let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_ref())
        .into_owned()
        .collect();

    let (header_id, header_secret) = parse_basic_auth(&headers).unzip();
    let client_id = params.get("client_id").cloned().or(header_id);
    let client_secret = params.get("client_secret").cloned().or(header_secret);
    let grant_type = params.get("grant_type").cloned().unwrap_or_default();

    state.recorded_tokens.lock().unwrap().push(RecordedToken {
        grant_type: Some(grant_type.clone()),
        code_verifier: params.get("code_verifier").cloned(),
        client_id,
        client_secret,
        resource: params.get("resource").cloned(),
    });

    if grant_type == "authorization_code" {
        let res = json!({
            "access_token": state.initial_access_token.clone(),
            "token_type": "Bearer",
            "expires_in": state.initial_expires_in,
            "refresh_token": state.initial_refresh_token.clone(),
            "scope": "offline_access"
        });
        return (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            res.to_string(),
        )
            .into_response();
    }

    if grant_type == "refresh_token" {
        return handle_refresh_token(&state).await;
    }

    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({"error": "unsupported_grant_type"}).to_string(),
    )
        .into_response()
}

pub async fn oauth_userinfo(
    State(state): State<Arc<MockOAuthServerState>>,
    headers: HeaderMap,
) -> Response {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let token = auth.and_then(|a| a.strip_prefix("Bearer "));
    let valid = token.is_some_and(|t| state.valid_tokens.lock().unwrap().contains(t));
    if !valid {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({
            "sub": "alice-sub",
            "email": "alice@example.test",
            "preferred_username": "alice"
        })
        .to_string(),
    )
        .into_response()
}

pub async fn mcp_auth_middleware(
    State(state): State<Arc<MockOAuthServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if state.simulate_403.load(Ordering::SeqCst) {
        return (
            StatusCode::FORBIDDEN,
            [
                (axum::http::header::CONTENT_TYPE, "application/json"),
                (
                    axum::http::header::WWW_AUTHENTICATE,
                    r#"Bearer error="insufficient_scope", error_description="missing scope", scope="admin""#,
                ),
            ],
            json!({"error": "insufficient_scope"}).to_string(),
        )
            .into_response();
    }

    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).to_string());

    if let Some(token) = auth_header {
        let rejected = state.rejected_tokens.lock().unwrap();
        if rejected.contains(&token) {
            return (
                StatusCode::UNAUTHORIZED,
                [
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                    (
                        axum::http::header::WWW_AUTHENTICATE,
                        r#"Bearer error="invalid_token", error_description="token expired""#,
                    ),
                ],
                json!({"error": "invalid_token"}).to_string(),
            )
                .into_response();
        }
    }

    next.run(req).await
}

pub struct MockOAuthServer {
    pub url: String,
    pub state: Arc<MockOAuthServerState>,
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl MockOAuthServer {
    pub async fn spawn(initial_expires_in: u64) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let base_url = format!("http://127.0.0.1:{port}");

        let initial_access = generate_secret("access-tok");
        let initial_refresh = generate_secret("refresh-tok");
        let mut valid_tokens = HashSet::new();
        valid_tokens.insert(initial_access.clone());

        let state = Arc::new(MockOAuthServerState {
            base_url: base_url.clone(),
            dcr_client_id: generate_secret("dcr-client"),
            dcr_client_secret: generate_secret("dcr-secret"),
            auth_code: generate_secret("auth-code"),
            initial_access_token: initial_access,
            initial_refresh_token: initial_refresh,
            initial_expires_in,
            refresh_expires_in: AtomicU64::new(3600),
            dcr_422: AtomicBool::new(false),
            reject_authorization: AtomicBool::new(false),
            invalid_grant: AtomicBool::new(false),
            transient_refresh_error: AtomicBool::new(false),
            simulate_403: AtomicBool::new(false),
            rejected_tokens: Mutex::new(HashSet::new()),
            refresh_count: AtomicUsize::new(0),
            dcr_count: AtomicUsize::new(0),
            token_request_count: AtomicUsize::new(0),
            mcp_request_count: AtomicUsize::new(0),
            recorded_auths: Mutex::new(Vec::new()),
            recorded_tokens: Mutex::new(Vec::new()),
            refresh_pause_tx: Mutex::new(None),
            refresh_resume_rx: Mutex::new(None),
            valid_tokens: Mutex::new(valid_tokens),
            rotated_counter: AtomicUsize::new(0),
        });

        let mcp_handler = MockMcpHandler {
            state: state.clone(),
        };
        let shutdown = CancellationToken::new();
        let mcp_config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(shutdown.child_token())
            .disable_allowed_hosts();
        let mcp_service = StreamableHttpService::new(
            move || Ok(mcp_handler.clone()),
            Arc::new(NeverSessionManager::default()),
            mcp_config,
        );

        let mcp_router = Router::new().fallback_service(mcp_service).layer(
            axum::middleware::from_fn_with_state(state.clone(), mcp_auth_middleware),
        );

        let app = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(oauth_protected_resource),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(oauth_protected_resource),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(oauth_as_metadata),
            )
            .route("/.well-known/openid-configuration", get(oauth_as_metadata))
            .route("/oauth/register", post(oauth_register))
            .route("/oauth/authorize", get(oauth_authorize))
            .route("/oauth/token", post(oauth_token))
            .route("/oauth/userinfo", get(oauth_userinfo))
            .nest_service("/mcp", mcp_router)
            .with_state(state.clone());

        let shutdown_wait = shutdown.clone();
        let join = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_wait.cancelled().await;
                })
                .await;
            if let Err(error) = result {
                log::error!("mock oauth server error: {error}");
            }
        });

        Ok(Self {
            url: format!("{base_url}/mcp"),
            state,
            shutdown,
            join,
        })
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.join.await;
    }
}

pub struct Instance {
    pub server: NativeServer,
    pub http_base: String,
    pub shutdown: CancellationToken,
}

pub struct SharedNats {
    pub _keys: tempfile::TempDir,
    pub config: NativeConfig,
    pub client: async_nats::Client,
    pub nats: harnx_test_bins::NatsServerHandle,
}

pub async fn shared_nats(upstream_url: &str, extra: &[&str]) -> SharedNats {
    let nats = harnx_test_bins::spawn_nats_server()
        .await
        .expect("spawn NATS")
        .expect("NATS tests require nats-server; set NATS_SERVER_BIN (no silent skips)");
    let client = async_nats::connect(nats.url()).await.expect("connect NATS");
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let mut args = vec!["--auth-methods", "oauth", "--oauth-scope", "offline_access"];
    args.extend(extra.iter().copied());
    let cli = native_cli(upstream_url, &key_file, &args);
    SharedNats {
        _keys: keys,
        config: config_from_cli(&cli),
        client,
        nats,
    }
}

impl Instance {
    pub async fn spawn(config: NativeConfig, client: &async_nats::Client) -> Self {
        let shutdown = CancellationToken::new();
        let server = start_native_on(config, client, shutdown.clone())
            .await
            .expect("start instance");
        let http_base = format!("http://{}", server.local_addr);
        Self {
            server,
            http_base,
            shutdown,
        }
    }
}

pub async fn start_instance(shared: &SharedNats) -> Instance {
    Instance::spawn(shared.config.clone(), &shared.client).await
}

pub struct OAuthFixture {
    pub mock: MockOAuthServer,
    pub shared: SharedNats,
    pub instance: Instance,
}

impl OAuthFixture {
    pub async fn spawn(initial_expires_in: u64) -> Self {
        let mock = MockOAuthServer::spawn(initial_expires_in)
            .await
            .expect("spawn mock");
        let shared = shared_nats(&mock.url, &[]).await;
        let instance = start_instance(&shared).await;
        Self {
            mock,
            shared,
            instance,
        }
    }

    pub async fn shutdown(self) {
        self.instance.shutdown.cancel();
        self.mock.shutdown().await;
    }

    pub fn toolset(&self) -> &RemoteCredentialsToolset {
        &self.instance.server.toolset
    }

    pub fn lifecycle(&self) -> &Lifecycle {
        &self.instance.server.lifecycle
    }

    pub async fn spawn_peer(&self) -> Instance {
        start_instance(&self.shared).await
    }

    pub async fn connect_alice(&self) -> (String, u64) {
        complete_instance_connect(&self.instance, "alice").await
    }
}

pub async fn start_connect(
    toolset: &RemoteCredentialsToolset,
    user: Option<&str>,
) -> (ToolInputRequired, Value) {
    let error = invoke(toolset, "connect", user, &[], Value::Null)
        .await
        .expect_err("connect starts a url challenge");
    url_challenge(error)
}

pub fn extract_oauth_link(html: &str) -> Option<String> {
    html.split("<a href=\"")
        .nth(1)?
        .split('"')
        .next()
        .map(str::to_string)
}

pub async fn drive_oauth_connect(
    http_base: &str,
    challenge_id: &str,
    extra_headers: &[(&str, &str)],
) -> Result<(u16, String)> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow::anyhow!("client build failed"))?;

    let mut req = http.get(format!("{http_base}/connect/{challenge_id}/oauth"));
    for (name, val) in extra_headers {
        req = req.header(*name, *val);
    }
    let res = req
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("oauth start request failed"))?;
    let status = res.status().as_u16();
    if status != 307 && status != 302 && status != 303 {
        let body = res
            .text()
            .await
            .map_err(|_| anyhow::anyhow!("read oauth start body failed"))?;
        return Ok((status, body));
    }
    let authorize_url = res
        .headers()
        .get(axum::http::header::LOCATION)
        .ok_or_else(|| anyhow::anyhow!("missing Location header"))?
        .to_str()
        .map_err(|_| anyhow::anyhow!("invalid Location header utf8"))?;

    let auth_res = http
        .get(authorize_url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("authorize request failed"))?;
    let auth_status = auth_res.status().as_u16();
    if auth_status != 307 && auth_status != 302 && auth_status != 303 {
        let body = auth_res
            .text()
            .await
            .map_err(|_| anyhow::anyhow!("read authorize body failed"))?;
        return Ok((auth_status, body));
    }
    let callback_url = auth_res
        .headers()
        .get(axum::http::header::LOCATION)
        .ok_or_else(|| anyhow::anyhow!("missing callback Location header"))?
        .to_str()
        .map_err(|_| anyhow::anyhow!("invalid callback Location header utf8"))?;

    let mut cb_url =
        Url::parse(callback_url).map_err(|_| anyhow::anyhow!("failed to parse callback URL"))?;
    let target_base =
        Url::parse(http_base).map_err(|_| anyhow::anyhow!("failed to parse target base URL"))?;
    let _ = cb_url.set_host(target_base.host_str());
    let _ = cb_url.set_port(target_base.port());
    let mut cb_req = http.get(cb_url.as_str());
    for (name, val) in extra_headers {
        cb_req = cb_req.header(*name, *val);
    }
    let cb_res = cb_req
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("callback request failed"))?;
    let cb_status = cb_res.status().as_u16();
    let body = cb_res
        .text()
        .await
        .map_err(|_| anyhow::anyhow!("read callback body failed"))?;
    Ok((cb_status, body))
}

pub fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

pub async fn delete_registration_with_retry(
    handle: &harnx_toolset_server::RegistrationHandle,
    audience: &harnx_toolset_server::HandleAudience,
    expected_conn_id: &str,
) {
    let outcome = crate::native_common::delete_registration_checked(
        handle,
        audience,
        expected_conn_id,
        expected_conn_id,
    )
    .await;
    assert_eq!(outcome, harnx_toolset_server::ConditionalDelete::Deleted);
}

pub async fn start_connect_flow(toolset: &RemoteCredentialsToolset, user: &str) -> (String, u64) {
    let (_challenge, challenge_id, round) = start_connect_with_challenge(toolset, user).await;
    (challenge_id, round)
}

pub async fn continue_connect(
    toolset: &RemoteCredentialsToolset,
    user: &str,
    challenge_id: &str,
    round: u64,
) -> Value {
    invoke(
        toolset,
        "connect",
        Some(user),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect("continue")
}

pub async fn complete_instance_connect(instance: &Instance, user: &str) -> (String, u64) {
    let (challenge_id, round) = start_connect_flow(&instance.server.toolset, user).await;
    let (status, body) = drive_oauth_connect(&instance.http_base, &challenge_id, &[])
        .await
        .expect("connect");
    assert_eq!(status, 200, "{body}");
    let _ = continue_connect(&instance.server.toolset, user, &challenge_id, round).await;
    (challenge_id, round)
}

pub async fn wait_for_stored_registration(
    handle: &RegistrationHandle,
    aud: &HandleAudience,
    timeout: Duration,
) -> StoredRegistration {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(r) = stored_registration(handle, aud).await {
            return r;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn invoke_tool(
    toolset: &RemoteCredentialsToolset,
    user: &str,
    tool: &str,
) -> Result<Value, ToolInvokeError> {
    invoke(toolset, tool, Some(user), &[], Value::Null).await
}

pub struct EncryptedBuckets {
    pub creds: async_nats::jetstream::kv::Store,
    pub flows: async_nats::jetstream::kv::Store,
    pub catalogs: async_nats::jetstream::kv::Store,
}

pub async fn open_encrypted_buckets(client: &async_nats::Client) -> EncryptedBuckets {
    let js = async_nats::jetstream::new(client.clone());
    let (creds, flows, catalogs) = tokio::try_join!(
        js.get_key_value(CREDENTIALS_BUCKET),
        js.get_key_value(FLOWS_BUCKET),
        js.get_key_value(CATALOGS_BUCKET),
    )
    .expect("open kv buckets");
    EncryptedBuckets {
        creds,
        flows,
        catalogs,
    }
}

pub async fn start_connect_with_challenge(
    toolset: &RemoteCredentialsToolset,
    user: &str,
) -> (ToolInputRequired, String, u64) {
    let (challenge, state) = start_connect(toolset, Some(user)).await;
    let cid = state["challenge_id"].as_str().expect("id").to_string();
    let round = state["round"].as_u64().expect("round");
    (challenge, cid, round)
}

pub async fn fetch_connect_page(http_base: &str, challenge_id: &str) -> (String, Option<String>) {
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let res = http
        .get(connect_page_url(http_base, challenge_id))
        .send()
        .await
        .expect("get page");
    let html = res.text().await.expect("html");
    let csrf = extract_csrf(&html);
    (html, csrf)
}

pub fn assert_secrets_absent(
    secrets: &[(&str, String)],
    text_surfaces: &[(&str, String)],
    byte_surfaces: &[(&str, &[u8])],
) {
    for (sec_name, sec_val) in secrets {
        for (surf_name, surf_text) in text_surfaces {
            assert!(
                !surf_text.contains(sec_val.as_str()),
                "secret {sec_name} leaked in {surf_name}"
            );
        }
        for (surf_name, surf_bytes) in byte_surfaces {
            assert!(
                !bytes_contain(surf_bytes, sec_val.as_bytes()),
                "secret {sec_name} leaked in {surf_name}"
            );
        }
    }
}
