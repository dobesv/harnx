use super::*;
use reqwest::header::CONTENT_TYPE;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::time::Instant;
use url::{form_urlencoded, Url};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use futures::stream::{self, StreamExt as _};
use tokio::{
    sync::{mpsc, Barrier},
    task::JoinHandle,
};

const PRE_EXPIRY_WINDOW: Duration = Duration::from_secs(60);
const RETIRED_SECRET_GENERATIONS: usize = 4;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

fn lock<T>(lockable: &Mutex<T>) -> MutexGuard<'_, T> {
    lockable
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SECRET: &str = "test-client-secret+s&=p";

/// One streamed body chunk, or a mid-body transport failure.
type BodyChunk = Result<Bytes, std::io::Error>;

/// One scripted token-endpoint response. An exhausted script answers with
/// a long-lived token so plain reuse paths make no assumptions.
enum Step {
    Token {
        token: String,
        expires_in: Option<u64>,
    },
    Raw {
        status: u16,
        body: String,
    },
    Held {
        release: Arc<Barrier>,
        then: Box<Step>,
    },
    Redirect,
    /// Headers arrive, then the body stalls mid-payload forever.
    BodyStall {
        status: u16,
    },
    /// A complete valid JSON prefix, then a mid-body connection reset.
    BodyReset {
        status: u16,
    },
    /// Delay response headers before returning a body that stalls.
    DelayedBodyStall {
        status: u16,
        delay: Duration,
    },
    Stall,
}

struct Recorded {
    body: String,
    content_type: Option<String>,
    authorization: Option<String>,
}

struct TokenServer {
    requests: Mutex<Vec<Recorded>>,
    steps: Mutex<VecDeque<Step>>,
    arrivals: mpsc::UnboundedSender<()>,
}

fn token_response(token: &str, expires_in: Option<u64>) -> Response {
    let mut json = serde_json::json!({"access_token": token, "token_type": "Bearer"});
    if let Some(secs) = expires_in {
        json["expires_in"] = serde_json::Value::from(secs);
    }
    (StatusCode::OK, Json(json)).into_response()
}

fn respond(step: &Step) -> Response {
    match step {
        Step::Token { token, expires_in } => token_response(token, *expires_in),
        Step::Raw { status, body } => (
            StatusCode::from_u16(*status).expect("valid status"),
            body.clone(),
        )
            .into_response(),
        // Location and any redirect body are untrusted and never logged.
        Step::Redirect => (
            StatusCode::FOUND,
            [("location", "https://example.invalid/leak?secret=x")],
        )
            .into_response(),
        Step::Held { then, .. } => respond(then),
        // A body that stalls mid-payload must be caught by the whole
        // exchange deadline, not by the header phase alone.
        Step::BodyStall { status } => {
            let chunks: Vec<BodyChunk> = vec![Ok(Bytes::from_static(
                br#"{"access_token":"tok","token_type":"Bea"#,
            ))];
            let stream = stream::iter(chunks).chain(stream::pending());
            Response::builder()
                .status(StatusCode::from_u16(*status).expect("valid status"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stream))
                .expect("body response")
        }
        // A complete, valid JSON prefix followed by a connection reset: the
        // read failure must surface, and the parsed-prefix token must never
        // be published. The gap guarantees the prefix is already on the
        // wire — so the failure surfaces in the body read, not at send
        // time — even under load.
        Step::BodyReset { status } => {
            let stream = stream::iter(vec![Ok(Bytes::from_static(
                br#"{"access_token":"tok","token_type":"Bearer"}"#,
            ))])
            .chain(stream::once(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ))
            }));
            Response::builder()
                .status(StatusCode::from_u16(*status).expect("valid status"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stream))
                .expect("body response")
        }
        Step::DelayedBodyStall { .. } => unreachable!("delay must be awaited in the handler"),
        Step::Stall => unreachable!("stall must be awaited in the handler"),
    }
}

async fn token_endpoint(
    State(state): State<Arc<TokenServer>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // An exhausted script answers with a long-lived token so plain reuse
    // paths make no assumptions.
    let recorded = Recorded {
        body: String::from_utf8_lossy(&body).into_owned(),
        content_type: headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        authorization: headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    };
    let index = {
        let mut requests = lock(&state.requests);
        requests.push(recorded);
        requests.len() - 1
    };
    let _ = state.arrivals.send(());
    let step = lock(&state.steps).pop_front().unwrap_or(Step::Token {
        token: format!("token-{index}"),
        expires_in: Some(3600),
    });
    match step {
        Step::Held { release, then } => {
            release.wait().await;
            respond(&then)
        }
        Step::DelayedBodyStall { status, delay } => {
            tokio::time::sleep(delay).await;
            respond(&Step::BodyStall { status })
        }
        Step::Stall => std::future::pending::<Response>().await,
        step => respond(&step),
    }
}

struct TokenFixture {
    endpoint: Url,
    state: Arc<TokenServer>,
    server: JoinHandle<()>,
}

impl Drop for TokenFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn token_server(steps: Vec<Step>) -> (TokenFixture, mpsc::UnboundedReceiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
    let (arrivals, arrived) = mpsc::unbounded_channel();
    let state = Arc::new(TokenServer {
        requests: Mutex::new(Vec::new()),
        steps: Mutex::new(steps.into()),
        arrivals,
    });
    let app = Router::new()
        .route("/token", any(token_endpoint))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        TokenFixture {
            endpoint,
            state,
            server,
        },
        arrived,
    )
}

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn make_provider(endpoint: &Url, scopes: &[&str]) -> ClientCredentialsProvider {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), format!("{SECRET}\r\n")).unwrap();
    ClientCredentialsProvider::new(
        endpoint.clone(),
        "client-id".into(),
        file.path(),
        scopes.iter().map(|scope| scope.to_string()).collect(),
        test_client(),
    )
    .unwrap()
}

fn make_auth(endpoint: &Url, scopes: &[&str]) -> Arc<dyn UpstreamAuth> {
    Arc::new(make_provider(endpoint, scopes))
}

fn bearer(value: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {value}")).unwrap()
}

fn applied_header(auth: &dyn UpstreamAuth) -> HeaderValue {
    let mut config = StreamableHttpClientTransportConfig::with_uri("");
    auth.apply(&mut config);
    config
        .custom_headers
        .get(&AUTHORIZATION)
        .cloned()
        .expect("cached credentials")
}

async fn ensure_ok(auth: &Arc<dyn UpstreamAuth>) -> HeaderValue {
    auth.ensure_fresh().await.unwrap();
    applied_header(auth.as_ref())
}

#[tokio::test]
async fn first_request_lazily_acquires_then_reuses_cached_token() {
    let (fixture, _arrived) = token_server(Vec::new()).await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let first = ensure_ok(&auth).await;
    assert_eq!(first, bearer("token-0"));
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    let second = ensure_ok(&auth).await;
    assert_eq!(second, bearer("token-0"));
    assert_eq!(lock(&fixture.state.requests).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn refreshes_inside_exact_sixty_second_window() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "t0".into(),
            expires_in: Some(121),
        },
        Step::Token {
            token: "t1".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    assert_eq!(ensure_ok(&auth).await, bearer("t0"));
    // 121s remaining: still more than the 60s window.
    assert_eq!(ensure_ok(&auth).await, bearer("t0"));
    tokio::time::advance(PRE_EXPIRY_WINDOW).await;
    // 61s remaining: one second outside the window.
    assert_eq!(ensure_ok(&auth).await, bearer("t0"));
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    // 60s remaining: exactly inside the window, so refresh.
    assert_eq!(ensure_ok(&auth).await, bearer("t1"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn concurrent_expiry_refresh_shares_one_exchange() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "t0".into(),
            expires_in: Some(120),
        },
        Step::Token {
            token: "t1".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    assert_eq!(ensure_ok(&auth).await, bearer("t0"));
    tokio::time::advance(PRE_EXPIRY_WINDOW + Duration::from_secs(1)).await;
    let headers = join_concurrent(&auth, 8).await;
    for header in headers {
        assert_eq!(header, bearer("t1"));
    }
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

async fn join_concurrent(auth: &Arc<dyn UpstreamAuth>, count: usize) -> Vec<HeaderValue> {
    let mut tasks = Vec::new();
    for _ in 0..count {
        let auth = Arc::clone(auth);
        tasks.push(tokio::spawn(async move { ensure_ok(&auth).await }));
    }
    let mut headers = Vec::new();
    for task in tasks {
        headers.push(task.await.unwrap());
    }
    headers
}

#[tokio::test]
async fn concurrent_first_acquisition_shares_one_exchange_and_one_token() {
    let (fixture, _arrived) = token_server(Vec::new()).await;
    let auth = make_auth(&fixture.endpoint, &[]);
    for header in join_concurrent(&auth, 8).await {
        assert_eq!(header, bearer("token-0"));
    }
    assert_eq!(lock(&fixture.state.requests).len(), 1);
}

#[tokio::test]
async fn failing_attempt_shares_one_safe_error_across_waiting_callers_then_recovers() {
    // The body echoes the client secret and a token; the shared error must
    // carry neither, only the status and the allowlisted code.
    let body = format!(r#"{{"error":"invalid_client","error_description":"{SECRET} token-0"}}"#);
    let release = Arc::new(Barrier::new(2));
    let (fixture, mut arrived) = token_server(vec![
        Step::Held {
            release: release.clone(),
            then: Box::new(Step::Raw { status: 500, body }),
        },
        Step::Token {
            token: "recovered".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);

    let leader = tokio::spawn({
        let auth = Arc::clone(&auth);
        async move { auth.ensure_fresh().await }
    });
    arrived.recv().await.expect("leader reached the endpoint");

    // These polls run each joiner's admission path while the exchange is
    // held, then prove both have registered on the shared attempt.
    let mut first = auth.ensure_fresh();
    let mut second = auth.ensure_fresh();
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());

    release.wait().await;
    let leader_error = leader.await.unwrap().expect_err("leader attempt fails");
    let first_error = first.await.expect_err("first waiter shares failure");
    let second_error = second.await.expect_err("second waiter shares failure");
    let rendered_errors = [
        leader_error.to_string(),
        first_error.to_string(),
        second_error.to_string(),
    ];
    let expected = &rendered_errors[0];
    for rendered in &rendered_errors {
        assert!(rendered.contains("HTTP 500"), "status in {rendered}");
        assert!(rendered.contains("invalid_client"), "code in {rendered}");
        assert!(!rendered.contains(SECRET), "no secret leak: {rendered}");
        assert!(!rendered.contains("token-0"), "no token leak: {rendered}");
        assert_eq!(rendered, expected, "whole batch shares one error");
    }
    assert_eq!(lock(&fixture.state.requests).len(), 1);

    // A later independent call starts a new attempt and recovers.
    assert_eq!(ensure_ok(&auth).await, bearer("recovered"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn short_and_zero_expiry_tokens_are_reused_by_waiting_generation_then_refetched() {
    for expires_in in [15, 0] {
        let release = Arc::new(Barrier::new(2));
        let (fixture, mut arrived) = token_server(vec![
            Step::Held {
                release: release.clone(),
                then: Box::new(Step::Token {
                    token: "short-lived".into(),
                    expires_in: Some(expires_in),
                }),
            },
            Step::Token {
                token: "independent".into(),
                expires_in: Some(3600),
            },
        ])
        .await;
        let auth = make_auth(&fixture.endpoint, &[]);

        let leader = tokio::spawn({
            let auth = Arc::clone(&auth);
            async move { ensure_ok(&auth).await }
        });
        arrived.recv().await.expect("leader reached the endpoint");
        let mut joiner = auth.ensure_fresh();
        assert!(futures::poll!(&mut joiner).is_pending());
        release.wait().await;

        assert_eq!(leader.await.unwrap(), bearer("short-lived"));
        // The joiner was already waiting on the in-flight attempt, so it
        // reuses the just-published short-lived token.
        joiner.await.unwrap();
        assert_eq!(applied_header(auth.as_ref()), bearer("short-lived"));
        assert_eq!(lock(&fixture.state.requests).len(), 1);

        // A later independent check is inside the window and refetches.
        assert_eq!(ensure_ok(&auth).await, bearer("independent"));
        assert_eq!(lock(&fixture.state.requests).len(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn missing_expiry_refreshes_only_after_rejection() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "t0".into(),
            expires_in: None,
        },
        Step::Token {
            token: "t1".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let first = ensure_ok(&auth).await;
    assert_eq!(first, bearer("t0"));
    // No expires_in means the cache is always "fresh": ten days of
    // monotonic time never trigger a proactive refresh.
    tokio::time::advance(Duration::from_secs(10 * 24 * 3600)).await;
    assert_eq!(ensure_ok(&auth).await, bearer("t0"));
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    // A rejection of the cached header is the only refresh trigger.
    auth.refresh_after_rejection(Some(&first)).await.unwrap();
    assert_eq!(applied_header(auth.as_ref()), bearer("t1"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn rejection_of_replaced_header_reuses_without_exchange() {
    let (fixture, _arrived) = token_server(Vec::new()).await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let current = ensure_ok(&auth).await;
    // A 401 that names an already-replaced token installs the replacement
    // without a new exchange.
    auth.refresh_after_rejection(Some(&bearer("other-token")))
        .await
        .unwrap();
    assert_eq!(applied_header(auth.as_ref()), current);
    assert_eq!(lock(&fixture.state.requests).len(), 1);
}

#[tokio::test]
async fn rejection_of_current_header_exchanges_and_installs_replacement() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "t0".into(),
            expires_in: Some(3600),
        },
        Step::Token {
            token: "t1".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let rejected = ensure_ok(&auth).await;
    auth.refresh_after_rejection(Some(&rejected)).await.unwrap();
    assert_eq!(applied_header(auth.as_ref()), bearer("t1"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn concurrent_rejections_of_same_token_exchange_once_and_reuse_replacement() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "t0".into(),
            expires_in: Some(3600),
        },
        Step::Token {
            token: "t1".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let rejected = ensure_ok(&auth).await;

    let mut refreshes = Vec::new();
    for _ in 0..2 {
        let auth = Arc::clone(&auth);
        let rejected = rejected.clone();
        refreshes.push(tokio::spawn(async move {
            auth.refresh_after_rejection(Some(&rejected)).await
        }));
    }
    for refresh in refreshes {
        refresh.await.unwrap().unwrap();
    }
    assert_eq!(applied_header(auth.as_ref()), bearer("t1"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn invalid_access_tokens_are_rejected_with_safe_errors() {
    let cases: &[(&str, &str, &str)] = &[
        ("missing", r#"{"token_type":"Bearer"}"#, "no access_token"),
        (
            "non-string",
            r#"{"access_token":5,"token_type":"Bearer"}"#,
            "no access_token",
        ),
        (
            "empty",
            r#"{"access_token":"","token_type":"Bearer"}"#,
            "empty access_token",
        ),
        (
            "whitespace",
            r#"{"access_token":"  ","token_type":"Bearer"}"#,
            "empty access_token",
        ),
        (
            "header-invalid",
            r#"{"access_token":"leaky-token\u000d\u000ainjected","token_type":"Bearer"}"#,
            "not a valid header value",
        ),
    ];
    for (label, body, expected) in cases {
        let (fixture, _arrived) = token_server(vec![Step::Raw {
            status: 200,
            body: (*body).into(),
        }])
        .await;
        let auth = make_auth(&fixture.endpoint, &[]);
        let error = auth.ensure_fresh().await.expect_err(label).to_string();
        assert!(error.contains(expected), "{label}: {error}");
        // Nothing was published, so the cache stays empty.
        assert_eq!(lock(&fixture.state.requests).len(), 1);
    }
}

#[tokio::test]
async fn token_type_must_be_bearer_case_insensitively() {
    for (token_type, accepted) in [
        ("basic", false),
        ("mac", false),
        ("Bearer", true),
        ("bEaReR", true),
        (" bearer ", true),
    ] {
        let body = format!(r#"{{"access_token":"tok","token_type":"{token_type}"}}"#);
        let (fixture, _arrived) = token_server(vec![Step::Raw { status: 200, body }]).await;
        let auth = make_auth(&fixture.endpoint, &[]);
        let result = auth.ensure_fresh().await;
        assert_eq!(result.is_ok(), accepted, "token_type {token_type}");
        if accepted {
            assert_eq!(applied_header(auth.as_ref()), bearer("tok"));
        }
    }
    // A missing token_type is rejected.
    let (fixture, _arrived) = token_server(vec![Step::Raw {
        status: 200,
        body: r#"{"access_token":"tok"}"#.into(),
    }])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    assert_eq!(
        auth.ensure_fresh().await.unwrap_err().to_string(),
        "token response has no token_type"
    );
}

#[tokio::test]
async fn malformed_expires_in_is_rejected_including_clock_overflow() {
    let cases: &[(&str, &str, &str)] = &[
        ("negative", "-5", "invalid expires_in"),
        ("fractional", "1.5", "invalid expires_in"),
        ("string", "\"3600\"", "invalid expires_in"),
        ("clock overflow", "18446744073709551615", "clock range"),
    ];
    for (label, raw, expected) in cases {
        let body = format!(r#"{{"access_token":"tok","token_type":"Bearer","expires_in":{raw}}}"#);
        let (fixture, _arrived) = token_server(vec![Step::Raw { status: 200, body }]).await;
        let auth = make_auth(&fixture.endpoint, &[]);
        let error = auth.ensure_fresh().await.expect_err(label).to_string();
        assert!(error.contains(expected), "{label}: {error}");
    }
    // A large-but-representable expiry is accepted.
    let (fixture, _arrived) = token_server(vec![Step::Raw {
        status: 200,
        body: r#"{"access_token":"tok","token_type":"Bearer","expires_in":315360000}"#.into(),
    }])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    assert!(auth.ensure_fresh().await.is_ok());
}

#[tokio::test]
async fn form_body_encodes_grant_scopes_and_credentials() {
    let (fixture, _arrived) = token_server(Vec::new()).await;
    let auth = make_auth(&fixture.endpoint, &["scope-a", "scope b"]);
    auth.ensure_fresh().await.unwrap();
    // Copy the recorded request out before any further await.
    let (body, content_type, authorization) = {
        let recorded = lock(&fixture.state.requests);
        assert_eq!(recorded.len(), 1);
        (
            recorded[0].body.clone(),
            recorded[0].content_type.clone(),
            recorded[0].authorization.clone(),
        )
    };
    assert_eq!(
        content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    // No Authorization header is forwarded to the token endpoint.
    assert_eq!(authorization, None);
    let pairs: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("grant_type".into(), "client_credentials".into()),
            ("client_id".into(), "client-id".into()),
            ("client_secret".into(), SECRET.into()),
            ("scope".into(), "scope-a scope b".into()),
        ]
    );

    // Without scopes the scope pair is omitted entirely.
    let (fixture, _arrived) = token_server(Vec::new()).await;
    let auth = make_auth(&fixture.endpoint, &[]);
    auth.ensure_fresh().await.unwrap();
    let body = lock(&fixture.state.requests)[0].body.clone();
    let pairs: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    assert!(!pairs.iter().any(|(name, _)| name == "scope"));
    assert_eq!(
        pairs[0],
        ("grant_type".to_string(), "client_credentials".to_string())
    );
}

#[tokio::test]
async fn error_diagnostics_are_bounded_and_allowlisted() {
    // (label, status, body, exact safe diagnostic)
    let cases: &[(&str, u16, &str, &str)] = &[
        (
            "allowlisted code",
            400,
            r#"{"error":"invalid_client","error_description":"prose with test-client-secret"}"#,
            "token endpoint returned HTTP 400 Bad Request (invalid_client)",
        ),
        (
            "unknown code",
            400,
            r#"{"error":"vendor_special","error_description":"prose"}"#,
            "token endpoint returned HTTP 400 Bad Request",
        ),
        (
            "non-json",
            503,
            "totally not json",
            "token endpoint returned HTTP 503 Service Unavailable",
        ),
        (
            "no error field",
            403,
            r#"{"detail":"prose"}"#,
            "token endpoint returned HTTP 403 Forbidden",
        ),
    ];
    for (label, status, body, expected) in cases {
        let (fixture, _arrived) = token_server(vec![Step::Raw {
            status: *status,
            body: (*body).to_string(),
        }])
        .await;
        let auth = make_auth(&fixture.endpoint, &[]);
        let error = auth.ensure_fresh().await.expect_err(label).to_string();
        assert_eq!(error, *expected, "{label}");
        if *label == "unknown code" {
            assert!(!error.contains("vendor_special"));
        }
        assert!(!error.contains("prose"), "{label}: {error}");
        assert!(!error.contains(SECRET), "{label}: {error}");
        assert!(
            error.len() < 160,
            "{label}: diagnostic must stay bounded: {error}"
        );
    }
}

#[tokio::test]
async fn oversized_body_fails_without_leaking() {
    let (fixture, _arrived) = token_server(vec![Step::Raw {
        status: 200,
        body: format!(
            r#"{{"access_token":"{}","token_type":"Bearer"}}"#,
            "A".repeat(2 * 1024 * 1024)
        ),
    }])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    assert_eq!(
        auth.ensure_fresh().await.unwrap_err().to_string(),
        "token response body exceeded the size limit"
    );
}

#[tokio::test]
async fn redirect_response_fails_closed() {
    let (fixture, _arrived) = token_server(vec![Step::Redirect, Step::Redirect]).await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let error = auth.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "token endpoint returned HTTP 302 Found; redirects are not allowed"
    );
    assert!(!error.contains("example.invalid"));
    assert!(!error.contains("secret=x"));
    // The redirect target was never contacted and no token was cached.
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    assert!(auth.ensure_fresh().await.is_err());
}

#[tokio::test]
async fn non_success_body_read_failure_retains_status_without_partial_content() {
    let (fixture, _arrived) = token_server(vec![
        Step::BodyReset { status: 503 },
        Step::Token {
            token: "recovered".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let error = auth.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "token endpoint returned HTTP 503 Service Unavailable; response body could not be read"
    );
    assert!(!error.contains("access_token"));
    assert!(!error.contains("Bearer tok"));
    assert!(!error.contains(&fixture.endpoint.to_string()));
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    assert_eq!(ensure_ok(&auth).await, bearer("recovered"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn stalled_endpoint_hits_token_timeout_then_recovers() {
    let (fixture, _arrived) = token_server(vec![
        Step::Stall,
        Step::Token {
            token: "recovered".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    // Paused clock: the 30s token-only deadline fires without wall-clock
    // waiting, before response headers exist, so no status is invented.
    let error = auth.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(error, "token exchange timed out");
    assert!(!error.contains("HTTP "));
    assert!(!error.contains(&fixture.endpoint.to_string()));
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    assert_eq!(ensure_ok(&auth).await, bearer("recovered"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn stalled_body_hits_the_same_token_timeout_then_recovers() {
    let (fixture, _arrived) = token_server(vec![
        Step::BodyStall { status: 200 },
        Step::Token {
            token: "recovered".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    // Headers arrived and the body stalled mid-payload: the deadline must
    // cover the whole operation, or the single-flight slot would hang with
    // every later caller parked behind it.
    assert_eq!(
        auth.ensure_fresh().await.unwrap_err().to_string(),
        "token exchange timed out"
    );
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    assert_eq!(ensure_ok(&auth).await, bearer("recovered"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn response_body_uses_remaining_total_exchange_deadline() {
    let (fixture, _arrived) = token_server(vec![
        Step::DelayedBodyStall {
            status: 503,
            delay: Duration::from_secs(20),
        },
        Step::Token {
            token: "recovered".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let started = Instant::now();
    let error = auth.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(Instant::now() - started, EXCHANGE_TIMEOUT);
    assert_eq!(
        error,
        "token endpoint returned HTTP 503 Service Unavailable; response body timed out"
    );
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    assert_eq!(ensure_ok(&auth).await, bearer("recovered"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn midbody_read_error_fails_and_never_publishes_the_partial_token() {
    let (fixture, _arrived) = token_server(vec![
        Step::BodyReset { status: 200 },
        Step::Token {
            token: "recovered".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    // The received bytes are a complete valid token payload, but the
    // connection reset mid-body: that is a read failure, not a success,
    // and never a size-cap error.
    let error = auth.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(error, "token response body could not be read");
    assert_ne!(error, "token response body exceeded the size limit");
    assert_eq!(lock(&fixture.state.requests).len(), 1);
    // The partial payload was never published: the next call exchanges
    // again and succeeds.
    assert_eq!(ensure_ok(&auth).await, bearer("recovered"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn oversized_error_status_body_retains_status_without_leaking() {
    // The oversized body echoes the client secret and provider prose. Keep
    // the known status and static cap reason, but never include body bytes.
    let body = format!(
        r#"{{"error":"invalid_client","error_description":"{SECRET}{}"}}"#,
        "A".repeat(2 * 1024 * 1024)
    );
    let (fixture, _arrived) = token_server(vec![Step::Raw { status: 500, body }]).await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let error = auth.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "token endpoint returned HTTP 500 Internal Server Error; response body exceeded the size limit"
    );
    assert!(!error.contains(SECRET));
    assert!(!error.contains("invalid_client"));
    assert!(!error.contains("A"));
}

#[tokio::test]
async fn rejection_without_header_is_mandatory_even_with_fresh_cache() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "t0".into(),
            expires_in: Some(3600),
        },
        Step::Token {
            token: "t1".into(),
            expires_in: Some(3600),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    assert_eq!(ensure_ok(&auth).await, bearer("t0"));
    // The cached token is fresh for an hour, but a rejection the adapter
    // could not identify (no header) is still a mandatory refresh.
    auth.refresh_after_rejection(None).await.unwrap();
    assert_eq!(applied_header(auth.as_ref()), bearer("t1"));
    assert_eq!(lock(&fixture.state.requests).len(), 2);
}

#[tokio::test]
async fn invalid_success_shapes_are_rejected_with_safe_errors() {
    let cases: &[(&str, &str, &str)] = &[
        ("non-json", "totally not json", "not valid JSON"),
        ("array", r#"["a"]"#, "not a JSON object"),
        (
            "null expires_in",
            r#"{"access_token":"tok","token_type":"Bearer","expires_in":null}"#,
            "invalid expires_in",
        ),
    ];
    for (label, body, expected) in cases {
        let (fixture, _arrived) = token_server(vec![Step::Raw {
            status: 200,
            body: (*body).into(),
        }])
        .await;
        let auth = make_auth(&fixture.endpoint, &[]);
        let error = auth.ensure_fresh().await.expect_err(label).to_string();
        assert!(error.contains(expected), "{label}: {error}");
        assert_eq!(lock(&fixture.state.requests).len(), 1);
    }
}

#[tokio::test]
async fn refresh_token_in_response_is_ignored() {
    let (fixture, _arrived) = token_server(vec![Step::Raw {
        status: 200,
        body: r#"{"access_token":"access-tok","token_type":"Bearer","expires_in":3600,"refresh_token":"refresh-leak-marker"}"#
            .into(),
    }])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    auth.ensure_fresh().await.expect("valid response");
    // The access token is cached; the refresh token is ignored entirely.
    assert_eq!(applied_header(auth.as_ref()), bearer("access-tok"));
}

#[tokio::test]
async fn network_error_is_static_and_hides_the_url() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = Url::parse(&format!("http://{address}/token")).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), SECRET).unwrap();
    let provider = ClientCredentialsProvider::new(
        endpoint.clone(),
        "client-id".into(),
        file.path(),
        Vec::new(),
        test_client(),
    )
    .unwrap();
    let error = provider.ensure_fresh().await.unwrap_err().to_string();
    assert_eq!(error, "token endpoint request failed");
    assert!(!error.contains("HTTP "));
    assert!(!error.contains(&endpoint.to_string()));
    assert!(!error.contains(SECRET));
}

#[tokio::test]
async fn redaction_covers_current_retired_and_client_secret_forms() {
    let steps = (0..RETIRED_SECRET_GENERATIONS as u64 + 2)
        .map(|n| Step::Token {
            token: format!("gen-token-{n}"),
            expires_in: Some(3600),
        })
        .collect();
    let (fixture, _arrived) = token_server(steps).await;
    let provider = make_provider(&fixture.endpoint, &[]);
    provider.ensure_fresh().await.unwrap();

    // Rotate: the initial token plus one replacement per generation.
    for _ in 0..RETIRED_SECRET_GENERATIONS + 1 {
        let rejected = applied_header(&provider);
        provider
            .refresh_after_rejection(Some(&rejected))
            .await
            .unwrap();
    }
    // The client secret is redacted in raw and form-encoded form, and the
    // most recently retired token in raw and Bearer form.
    let recent = format!("gen-token-{}", RETIRED_SECRET_GENERATIONS + 1);
    let encoded_secret = form_urlencoded::byte_serialize(SECRET.as_bytes()).collect::<String>();
    assert_ne!(
        encoded_secret, SECRET,
        "test secret must change under form encoding"
    );
    for form in [
        SECRET.to_string(),
        encoded_secret,
        format!("Bearer {recent}"),
        recent.clone(),
    ] {
        assert!(!provider.redact(&form).contains(form.as_str()), "{form}");
    }
    // The oldest generation fell out of the bounded retired history.
    assert!(
        provider.redact("gen-token-0 tail").contains("gen-token-0"),
        "bounded retired history must forget old generations"
    );
    // Current and retired tokens stay redactable inside larger text.
    let echo = format!("x gen-token-1 y {SECRET} z {recent} w");
    let redacted = provider.redact(&echo);
    assert!(!redacted.contains(SECRET));
    assert!(!redacted.contains("gen-token-1"));
    assert!(!redacted.contains(&recent));
}

mod refresh;
