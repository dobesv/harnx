use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};

#[derive(Default)]
struct TestAuth(AtomicUsize);

#[async_trait::async_trait]
impl UpstreamAuth for TestAuth {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        let mut value = HeaderValue::from_static(if self.0.load(Ordering::SeqCst) == 0 {
            "Bearer before-refresh"
        } else {
            "Bearer after-refresh"
        });
        value.set_sensitive(true);
        config.custom_headers.insert(AUTHORIZATION, value);
    }
    fn refreshable(&self) -> bool {
        true
    }
    async fn refresh_after_rejection(&self, _rejected: Option<&HeaderValue>) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    status: StatusCode,
    challenge: bool,
    always: bool,
}

async fn reply(
    State(mock): State<Mock>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let call = mock.calls.fetch_add(1, Ordering::SeqCst);
    let expected = if call == 0 {
        "Bearer before-refresh"
    } else {
        "Bearer after-refresh"
    };
    assert!(headers
        .get(AUTHORIZATION)
        .is_some_and(|value| value == expected));
    let status = if call == 0 || mock.always {
        mock.status
    } else {
        StatusCode::ACCEPTED
    };
    let mut response = (status, "provider-secret-marker").into_response();
    if mock.challenge {
        response.headers_mut().insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Bearer error=\"insufficient_scope\", scope=\"provider-secret-marker\"",
            ),
        );
    }
    response
}

async fn request(mock: Mock) -> (HttpResult<StreamableHttpPostResponse>, usize, usize) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/mcp", post(reply))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let auth = Arc::new(TestAuth::default());
    let client = AuthenticatedHttp {
        http: reqwest::Client::new(),
        auth: auth.clone(),
    };
    let message =
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"ping"}))
            .unwrap();
    let result = client
        .post_message(url.into(), message, None, None, Headers::new())
        .await;
    server.abort();
    (
        result,
        mock.calls.load(Ordering::SeqCst),
        auth.0.load(Ordering::SeqCst),
    )
}

fn mock(status: StatusCode, challenge: bool, always: bool) -> Mock {
    Mock {
        calls: Arc::new(AtomicUsize::new(0)),
        status,
        challenge,
        always,
    }
}

#[tokio::test]
async fn only_http_401_retries_once_even_without_authenticate_header() {
    for challenge in [false, true] {
        let (result, calls, refreshes) =
            request(mock(StatusCode::UNAUTHORIZED, challenge, false)).await;
        assert!(
            matches!(result, Ok(StreamableHttpPostResponse::Accepted)),
            "challenge={challenge} calls={calls} refreshes={refreshes} safe_error={:?}",
            result.err()
        );
        assert_eq!((calls, refreshes), (2, 1));
    }
    let (result, calls, refreshes) = request(mock(StatusCode::UNAUTHORIZED, true, true)).await;
    assert!(result.is_err());
    assert_eq!((calls, refreshes), (2, 1));
}

#[tokio::test]
async fn scope_and_server_errors_are_redacted_and_never_retried() {
    for status in [StatusCode::FORBIDDEN, StatusCode::INTERNAL_SERVER_ERROR] {
        let (result, calls, refreshes) =
            request(mock(status, status == StatusCode::FORBIDDEN, true)).await;
        let error = result.err().unwrap();
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("provider-secret-marker"));
        if status == StatusCode::FORBIDDEN {
            assert!(rendered.contains("insufficient_scope"));
        }
        assert_eq!((calls, refreshes), (1, 0));
    }
}
