use std::{future::Future, time::Duration};

use harnx_http_auth::{
    tls::{credentialed_client_builder, TlsPaths},
    AccessToken, ClientCredentialsProvider,
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const SECRET: &str = "test-secret +/ä";
const AWAIT_REQUEST: Duration = Duration::from_secs(5);

struct ServiceAccount {
    endpoint: TcpListener,
    provider: ClientCredentialsProvider,
}

impl ServiceAccount {
    async fn new() -> Self {
        let endpoint = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", endpoint.local_addr().unwrap())
            .parse()
            .unwrap();
        let secret = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(secret.path(), format!("{SECRET}\r\n")).unwrap();
        let http = credentialed_client_builder(&TlsPaths {
            cert: None,
            key: None,
            ca: None,
        })
        .unwrap()
        .build()
        .unwrap();
        let provider =
            ClientCredentialsProvider::new(url, "my-app".into(), secret.path(), vec![], http)
                .unwrap();
        Self { endpoint, provider }
    }

    async fn request(&self) -> TcpStream {
        timeout(AWAIT_REQUEST, async {
            let (socket, _) = self.endpoint.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut headers = String::new();
            loop {
                let start = headers.len();
                assert_ne!(reader.read_line(&mut headers).await.unwrap(), 0);
                if &headers[start..] == "\r\n" {
                    break;
                }
            }
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .unwrap()
                .1
                .trim()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            assert!(headers.starts_with("POST /token HTTP/1.1\r\n"));
            assert_eq!(
                String::from_utf8(body).unwrap(),
                "grant_type=client_credentials&client_id=my-app&client_secret=test-secret+%2B%2F%C3%A4"
            );
            reader.into_inner()
        })
        .await
        .expect("token POST did not arrive")
    }

    async fn exchange(
        &self,
        call: impl Future<Output = anyhow::Result<AccessToken>>,
        body: String,
    ) -> AccessToken {
        let (token, ()) = tokio::join!(call, async {
            respond(self.request().await, "200 OK", body).await;
        });
        token.unwrap()
    }

    fn assert_no_token_post(&self) {
        assert!(self.endpoint.accept().now_or_never().is_none());
    }
}

use futures_util::FutureExt as _;

fn token_body(token: &str, expiry: Option<u64>) -> String {
    let mut body = json!({"access_token": token, "token_type": "Bearer"});
    if let Some(seconds) = expiry {
        body["expires_in"] = json!(seconds);
    }
    body.to_string()
}

async fn respond(mut socket: TcpStream, status: &str, body: String) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn reject_only_retires_exact_generation_even_when_bearer_bytes_repeat() {
    let mock = ServiceAccount::new().await;
    mock.assert_no_token_post();
    let auth = &mock.provider;
    let first = mock
        .exchange(auth.token(), token_body("same-token", None))
        .await;
    assert!(auth.reject(&first));
    assert!(!auth.reject(&first));
    mock.assert_no_token_post();
    let second = mock
        .exchange(auth.token(), token_body("same-token", None))
        .await;
    assert_eq!(first.authorization_header(), second.authorization_header());
    assert!(second.authorization_header().is_sensitive());
    assert!(!format!("{second:?}").contains("same-token"));
    assert!(!auth.reject(&first), "late response for first generation");
    let replay = auth.renew_rejected(&first).now_or_never().unwrap().unwrap();
    assert!(auth.reject(&replay), "replay used second generation");
    mock.assert_no_token_post();
}

#[tokio::test]
async fn late_replay_uses_near_expiry_replacement_without_proactive_refresh() {
    let mock = ServiceAccount::new().await;
    let auth = &mock.provider;
    let first = mock
        .exchange(auth.token(), token_body("first+/=", None))
        .await;
    let fresh = mock
        .exchange(
            auth.renew_rejected(&first),
            token_body("second+/=", Some(40)),
        )
        .await;
    let replay = auth.renew_rejected(&first).now_or_never().unwrap().unwrap();
    assert_eq!(fresh.authorization_header(), replay.authorization_header());
    mock.assert_no_token_post();
    let echo = "Bearer first+/= | first%2B%2F%3D | Bearer+second%2b%2f%3d | test-secret+%2B%2F%C3%A4 | test-secret%20%2B%2F%C3%A4 | regular data";
    assert_eq!(
        auth.redact(echo),
        "[redacted] | [redacted] | [redacted] | [redacted] | [redacted] | regular data"
    );
}

// Poll callers before releasing the response so both must share this exact
// attempt, even if another call starts an independent exchange later.
async fn exchange_after_cancellation(
    mock: &ServiceAccount,
    status: &str,
    body: String,
) -> [anyhow::Result<AccessToken>; 2] {
    let auth = &mock.provider;
    let mut leader = Box::pin(auth.token());
    assert!(futures_util::poll!(&mut leader).is_pending());
    let socket = mock.request().await;
    drop(leader);

    let mut abandoned_waiter = Box::pin(auth.token());
    let mut first = Box::pin(auth.token());
    let mut second = Box::pin(auth.token());
    assert!(futures_util::poll!(&mut abandoned_waiter).is_pending());
    assert!(futures_util::poll!(&mut first).is_pending());
    assert!(futures_util::poll!(&mut second).is_pending());
    drop(abandoned_waiter);

    respond(socket, status, body).await;
    let (first, second) = timeout(AWAIT_REQUEST, async { tokio::join!(first, second) })
        .await
        .expect("cancelled waiters stranded the shared attempt");
    mock.assert_no_token_post();
    [first, second]
}

#[tokio::test]
async fn cancelled_callers_do_not_abort_a_successful_exchange() {
    let mock = ServiceAccount::new().await;
    let tokens = exchange_after_cancellation(&mock, "200 OK", token_body("shared", None)).await;
    for token in tokens {
        assert_eq!(token.unwrap().authorization_header(), "Bearer shared");
    }
    assert_eq!(
        mock.provider
            .token()
            .now_or_never()
            .unwrap()
            .unwrap()
            .authorization_header(),
        "Bearer shared"
    );
}

#[tokio::test]
async fn cancelled_callers_share_failure_and_next_call_can_recover() {
    let mock = ServiceAccount::new().await;
    let body = json!({"error": "invalid_client", "error_description": SECRET}).to_string();
    let failed = exchange_after_cancellation(&mock, "400 Bad Request", body).await;
    for error in failed {
        assert_eq!(
            error.unwrap_err().to_string(),
            "token endpoint returned HTTP 400 Bad Request (invalid_client)"
        );
    }
    let recovered = mock
        .exchange(mock.provider.token(), token_body("recovered", None))
        .await;
    assert_eq!(recovered.authorization_header(), "Bearer recovered");
}
