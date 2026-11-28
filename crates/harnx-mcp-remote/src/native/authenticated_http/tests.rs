use super::*;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst},
        Mutex,
    },
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::State,
    http::{header::WWW_AUTHENTICATE, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use futures::StreamExt;
use tokio::{sync::Barrier, task::JoinHandle};

const SHORT_TTL: u64 = 15;
const SESSION_ID: &str = "session-a";
const LAST_EVENT_ID: &str = "event-42";
const CUSTOM_HEADER: &str = "x-custom-header";
const CUSTOM_VALUE: &str = "keep-me";
const SECRET_MARKER: &str = "provider-secret-marker";
const TIMEOUT: Duration = Duration::from_secs(15);

// No clock or token endpoint needed here: each simulated exchange advances
// the token generation and leaves the replacement inside the freshness window.
struct TestAuth {
    ttl_secs: AtomicU64,
    exchanges: AtomicUsize,
    freshness_checks: AtomicUsize,
    rejections: AtomicUsize,
    fail_replay_stamp: AtomicBool,
}

impl Default for TestAuth {
    fn default() -> Self {
        Self {
            ttl_secs: AtomicU64::new(3600),
            exchanges: AtomicUsize::new(0),
            freshness_checks: AtomicUsize::new(0),
            rejections: AtomicUsize::new(0),
            fail_replay_stamp: AtomicBool::new(false),
        }
    }
}

fn bearer(generation: usize) -> HeaderValue {
    let mut value = HeaderValue::from_str(&format!("Bearer test-token-{generation}")).unwrap();
    value.set_sensitive(true);
    value
}

impl TestAuth {
    fn exchange(&self) {
        self.exchanges.fetch_add(1, SeqCst);
        self.ttl_secs.store(SHORT_TTL, SeqCst);
    }

    fn current(&self) -> HeaderValue {
        bearer(self.exchanges.load(SeqCst))
    }

    fn assert_counts(&self, freshness_checks: usize, rejections: usize, exchanges: usize) {
        assert_eq!(self.freshness_checks.load(SeqCst), freshness_checks);
        assert_eq!(self.rejections.load(SeqCst), rejections);
        assert_eq!(self.exchanges.load(SeqCst), exchanges);
    }
}

#[async_trait::async_trait]
impl UpstreamAuth for TestAuth {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        config.custom_headers.insert(AUTHORIZATION, self.current());
    }

    fn apply_for_request(
        &self,
        config: &mut StreamableHttpClientTransportConfig,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.fail_replay_stamp.load(SeqCst) || self.rejections.load(SeqCst) == 0,
            "cached credentials invalidated"
        );
        self.apply(config);
        Ok(())
    }

    async fn ensure_fresh(&self) -> anyhow::Result<()> {
        self.freshness_checks.fetch_add(1, SeqCst);
        if self.ttl_secs.load(SeqCst) <= 60 {
            self.exchange();
        }
        Ok(())
    }

    fn refreshable(&self) -> bool {
        true
    }

    async fn refresh_after_rejection(&self, rejected: Option<&HeaderValue>) -> anyhow::Result<()> {
        self.rejections.fetch_add(1, SeqCst);
        if rejected == Some(&self.current()) {
            self.exchange();
        }
        Ok(())
    }
}

struct Attempt {
    method: Method,
    headers: HeaderMap,
    body: Bytes,
}

struct Mock {
    attempts: Mutex<Vec<Attempt>>,
    status: StatusCode,
    challenge: Option<HeaderValue>,
    always_reject: bool,
    first_request: Option<Arc<Barrier>>,
}

impl Mock {
    fn new(status: StatusCode) -> Self {
        Self {
            attempts: Mutex::default(),
            status,
            challenge: None,
            always_reject: false,
            first_request: None,
        }
    }

    fn assert_attempts(&self, method: Method, generations: &[usize]) {
        let attempts = self.attempts.lock().unwrap();
        assert_eq!(attempts.len(), generations.len());
        for (attempt, generation) in attempts.iter().zip(generations) {
            assert_eq!(attempt.method, method);
            assert_eq!(attempt.headers[AUTHORIZATION], bearer(*generation));
            assert_eq!(attempt.headers.get_all(AUTHORIZATION).iter().count(), 1);
            assert_eq!(attempt.headers[CUSTOM_HEADER], CUSTOM_VALUE);
            assert_eq!(attempt.headers["mcp-session-id"], SESSION_ID);
            if method == Method::GET {
                assert_eq!(attempt.headers["last-event-id"], LAST_EVENT_ID);
            }
            if method == Method::POST {
                let received: serde_json::Value = serde_json::from_slice(&attempt.body).unwrap();
                assert_eq!(received, ping());
            }
        }
    }
}

async fn reply(
    State(mock): State<Arc<Mock>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let first = {
        let mut attempts = mock.attempts.lock().unwrap();
        let first = attempts.is_empty();
        attempts.push(Attempt {
            method: method.clone(),
            headers,
            body,
        });
        first
    };
    if first {
        if let Some(barrier) = &mock.first_request {
            barrier.wait().await; // old token arrived
            barrier.wait().await; // replacement installed
        }
    }
    if first || mock.always_reject {
        // A 401-looking body must not turn a different HTTP status into a retry.
        let body = format!("HTTP 401 Unauthorized: {SECRET_MARKER}");
        let mut response = (mock.status, body).into_response();
        if let Some(challenge) = &mock.challenge {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, challenge.clone());
        }
        return response;
    }
    if method == Method::GET {
        return ([("content-type", "text/event-stream")], "data: resumed\n\n").into_response();
    }
    StatusCode::ACCEPTED.into_response()
}

struct Fixture {
    mock: Arc<Mock>,
    auth: Arc<TestAuth>,
    client: AuthenticatedHttp,
    uri: Arc<str>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn start(mock: Mock) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
        let mock = Arc::new(mock);
        let app = Router::new()
            .route("/mcp", any(reply))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let auth = Arc::new(TestAuth::default());
        let client = AuthenticatedHttp {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(TIMEOUT)
                .build()
                .unwrap(),
            auth: auth.clone(),
        };
        Self {
            mock,
            auth,
            client,
            uri,
            server,
        }
    }

    async fn post(&self) -> HttpResult<StreamableHttpPostResponse> {
        self.client
            .post_message(
                self.uri.clone(),
                serde_json::from_value(ping()).unwrap(),
                Some(SESSION_ID.into()),
                None,
                custom_headers(),
            )
            .await
    }

    async fn send(&self, method: &Method) -> HttpResult<()> {
        match *method {
            Method::POST => self.post().await.map(drop),
            Method::GET => self.get().await.map(drop),
            _ => unreachable!("fixture only dispatches POST and GET here"),
        }
    }

    async fn get(&self) -> HttpResult<Events> {
        self.client
            .get_stream(
                self.uri.clone(),
                Some(SESSION_ID.into()),
                Some(LAST_EVENT_ID.into()),
                None,
                custom_headers(),
            )
            .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn ping() -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})
}

fn custom_headers() -> Headers {
    Headers::from([
        (
            HeaderName::from_static(CUSTOM_HEADER),
            HeaderValue::from_static(CUSTOM_VALUE),
        ),
        (
            AUTHORIZATION,
            HeaderValue::from_static("Bearer must-be-replaced"),
        ),
    ])
}

fn challenge() -> HeaderValue {
    HeaderValue::from_static(
        "Bearer error=\"insufficient_scope\", scope=\"provider-secret-marker\"",
    )
}

#[tokio::test]
async fn only_http_401_retries_once_even_without_authenticate_header() {
    for challenge in [None, Some(challenge())] {
        let fixture = Fixture::start(Mock {
            challenge,
            ..Mock::new(StatusCode::UNAUTHORIZED)
        })
        .await;
        let response = fixture.post().await.expect("POST replay should succeed");
        assert!(matches!(response, StreamableHttpPostResponse::Accepted));
        fixture.mock.assert_attempts(Method::POST, &[0, 1]);
        fixture.auth.assert_counts(1, 1, 1);
    }

    let fixture = Fixture::start(Mock {
        challenge: Some(challenge()),
        always_reject: true,
        ..Mock::new(StatusCode::UNAUTHORIZED)
    })
    .await;
    assert!(fixture.post().await.is_err());
    fixture.mock.assert_attempts(Method::POST, &[0, 1]);
    fixture.auth.assert_counts(1, 1, 1);
}

#[tokio::test]
async fn invalidation_before_stamping_prevents_replay_without_refetching() {
    for method in [Method::POST, Method::GET] {
        let fixture = Fixture::start(Mock::new(StatusCode::UNAUTHORIZED)).await;
        fixture.auth.fail_replay_stamp.store(true, SeqCst);
        let result = fixture.send(&method).await;
        let Err(StreamableHttpError::Client(SafeHttpError(message))) = result else {
            panic!("{method} must fail closed if replay credentials can't be stamped");
        };
        assert_eq!(message, "cached credentials invalidated");
        fixture.mock.assert_attempts(method, &[0]);
        fixture.auth.assert_counts(1, 1, 1);
    }
}

#[tokio::test]
async fn in_flight_rejection_replays_concurrent_replacement_without_another_exchange() {
    let barrier = Arc::new(Barrier::new(2));
    let fixture = Fixture::start(Mock {
        first_request: Some(barrier.clone()),
        ..Mock::new(StatusCode::UNAUTHORIZED)
    })
    .await;

    let (result, ()) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(fixture.post(), async {
            barrier.wait().await;
            fixture.auth.ttl_secs.store(SHORT_TTL, SeqCst);
            fixture.auth.ensure_fresh().await.unwrap();
            fixture.auth.assert_counts(2, 0, 1);
            barrier.wait().await;
        })
    })
    .await
    .expect("wait for in-flight rejection and replay");

    assert!(matches!(
        result.unwrap(),
        StreamableHttpPostResponse::Accepted
    ));
    fixture.mock.assert_attempts(Method::POST, &[0, 1]);
    fixture.auth.assert_counts(2, 1, 1);
}

#[tokio::test]
async fn sse_get_replays_one_401_and_keeps_resume_cursor() {
    for always_reject in [false, true] {
        let fixture = Fixture::start(Mock {
            always_reject,
            ..Mock::new(StatusCode::UNAUTHORIZED)
        })
        .await;
        let result = fixture.get().await;
        if always_reject {
            assert!(result.is_err());
        } else {
            let event = result.unwrap().next().await.unwrap().unwrap();
            assert_eq!(event.data.as_deref(), Some("resumed"));
        }
        fixture.mock.assert_attempts(Method::GET, &[0, 1]);
        fixture.auth.assert_counts(1, 1, 1);
    }
}

#[tokio::test]
async fn delete_checks_freshness_but_never_replays() {
    let fixture = Fixture::start(Mock::new(StatusCode::UNAUTHORIZED)).await;
    fixture.auth.ttl_secs.store(SHORT_TTL, SeqCst);
    let result = fixture
        .client
        .delete_session(
            fixture.uri.clone(),
            SESSION_ID.into(),
            None,
            custom_headers(),
        )
        .await;
    assert!(result.is_err());
    fixture.mock.assert_attempts(Method::DELETE, &[1]);
    fixture.auth.assert_counts(1, 0, 1);
}

#[tokio::test]
async fn replay_preserves_sensitive_authorization_and_unrelated_headers() {
    let client = AuthenticatedHttp {
        http: reqwest::Client::new(),
        auth: Arc::new(TestAuth::default()),
    };
    // Sensitivity is in-process metadata and can't be observed by the server.
    let initial = client.headers(custom_headers()).await.unwrap();
    let replay = client.rejected(&initial).await.unwrap();
    for (generation, headers) in [initial, replay].into_iter().enumerate() {
        assert_eq!(headers[&AUTHORIZATION], bearer(generation));
        assert!(headers[&AUTHORIZATION].is_sensitive());
        assert_eq!(
            headers[&HeaderName::from_static(CUSTOM_HEADER)],
            CUSTOM_VALUE
        );
    }
}

#[tokio::test]
async fn nontext_authenticate_header_fails_closed_for_both_401_and_403() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        for method in [Method::POST, Method::GET] {
            let fixture = Fixture::start(Mock {
                challenge: Some(HeaderValue::from_bytes(b"Bearer \xff").unwrap()),
                ..Mock::new(status)
            })
            .await;
            let result = fixture.send(&method).await;
            assert_eq!(
                result.unwrap_err().to_string(),
                "Client error: upstream HTTP request failed"
            );
            fixture.mock.assert_attempts(method, &[0]);
            fixture.auth.assert_counts(1, 0, 0);
        }
    }
}

#[tokio::test]
async fn scope_and_server_errors_are_redacted_and_never_retried() {
    for (status, has_scope_challenge) in [
        (StatusCode::FORBIDDEN, true),
        (StatusCode::FORBIDDEN, false),
        (StatusCode::INTERNAL_SERVER_ERROR, false),
    ] {
        for method in [Method::POST, Method::GET] {
            let fixture = Fixture::start(Mock {
                challenge: has_scope_challenge.then(challenge),
                ..Mock::new(status)
            })
            .await;
            let result = fixture.send(&method).await;
            let error = result.unwrap_err();
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(SECRET_MARKER));
            assert_eq!(rendered.contains("insufficient_scope"), has_scope_challenge);
            fixture.mock.assert_attempts(method, &[0]);
            fixture.auth.assert_counts(1, 0, 0);
        }
    }
}
