use anyhow::{anyhow, Result};
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::Credentials;
use axum::extract::State;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use serde::Serialize;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::net::TcpListener;
use uuid::Uuid;

struct AppState {
    bearer_token: String,
    creds_provider: Arc<dyn ProvideCredentials>,
    region: String,
}

mod cli;
mod toolset;
use cli::Args;

#[derive(Serialize)]
struct CredsResponse {
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "Token", skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(rename = "Expiration", skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
}

async fn build_app_state(args: &Args) -> Result<Arc<AppState>> {
    let mut builder = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(profile) = &args.profile {
        builder = builder.profile_name(profile);
    }

    let sdk_config = builder.load().await;
    let region = sdk_config
        .region()
        .map(|region| region.to_string())
        .unwrap_or_else(|| "us-east-1".to_string());
    let creds_provider: Arc<dyn ProvideCredentials> = Arc::new(
        sdk_config
            .credentials_provider()
            .ok_or_else(|| anyhow!("no credentials provider"))?,
    );

    Ok(Arc::new(AppState {
        bearer_token: Uuid::new_v4().to_string(),
        creds_provider,
        region,
    }))
}

async fn creds_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // AWS SDKs send the value of AWS_CONTAINER_AUTHORIZATION_TOKEN directly as the
    // Authorization header — no "Bearer " prefix per the container credentials spec.
    let actual = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    if actual != Some(state.bearer_token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match state.creds_provider.provide_credentials().await {
        Ok(credentials) => Json(creds_response(credentials)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to provide credentials: {err}"),
        )
            .into_response(),
    }
}

fn creds_response(credentials: Credentials) -> CredsResponse {
    CredsResponse {
        access_key_id: credentials.access_key_id().to_string(),
        secret_access_key: credentials.secret_access_key().to_string(),
        token: credentials.session_token().map(ToString::to_string),
        expiration: credentials.expiry().and_then(format_expiration),
    }
}

fn format_expiration(expiry: SystemTime) -> Option<String> {
    let datetime: chrono::DateTime<chrono::Utc> = expiry.into();
    Some(datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

async fn start_server(state: Arc<AppState>, listener: TcpListener) -> Result<u16> {
    let port = listener.local_addr()?.port();
    let router = Router::new()
        .route("/creds", get(creds_handler))
        .layer(axum::middleware::from_fn(
            harnx_metrics::http_metrics_middleware,
        ))
        .with_state(state);

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).into_future().await {
            eprintln!("harnx-aws-creds: server error: {err}");
        }
    });

    Ok(port)
}

fn main() -> Result<()> {
    const MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;
    std::thread::Builder::new()
        .name("aws-creds-main".into())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(Box::pin(run()))
        })?
        .join()
        .map_err(|_| anyhow!("AWS credentials server thread panicked"))?
}

// qual:allow(iosp) — the listing branch only picks which toolset to build
async fn run() -> Result<()> {
    let args = Args::parse();
    if args.list_tools {
        // Tool names don't depend on credentials, so listing must not need any.
        let toolset = toolset::AwsCredentialsToolset::new(0, "", "");
        return Box::pin(harnx_toolset_server::run_toolset_main(toolset)).await;
    }
    let state = build_app_state(&args).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = start_server(Arc::clone(&state), listener).await?;
    let toolset = toolset::AwsCredentialsToolset::new(port, &state.bearer_token, &state.region);
    Box::pin(harnx_toolset_server::run_toolset_main(toolset)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::provider::future;
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[derive(Debug)]
    struct MockProvider;

    impl ProvideCredentials for MockProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(Ok(Credentials::new(
                "AKIATEST",
                "SECRET",
                Some("TOKEN".to_string()),
                None,
                "test",
            )))
        }
    }

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            bearer_token: "testtoken".to_string(),
            creds_provider: Arc::new(MockProvider),
            region: "us-east-1".to_string(),
        })
    }

    async fn spawn_test_server(state: Arc<AppState>) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new()
            .route("/creds", get(creds_handler))
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (port, handle)
    }

    async fn read_http_response(
        port: u16,
        authorization_header: Option<&str>,
    ) -> (axum::http::StatusCode, Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut request =
            format!("GET /creds HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        if let Some(header) = authorization_header {
            request.push_str(&format!("Authorization: {header}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.lines().next().unwrap();
        let status_code = status
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let body = if body.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(body.trim()).unwrap()
        };

        (axum::http::StatusCode::from_u16(status_code).unwrap(), body)
    }

    async fn creds_request_with(authorization: Option<&str>) -> (StatusCode, Value) {
        let state = test_state();
        let (port, handle) = spawn_test_server(state.clone()).await;
        let result = read_http_response(port, authorization).await;
        handle.abort();
        let _ = handle.await;
        result
    }

    #[tokio::test]
    async fn creds_200_valid_token() {
        let state = test_state();
        let token = state.bearer_token.clone();
        let (port, handle) = spawn_test_server(state).await;
        let (status, body) = read_http_response(port, Some(&token)).await;
        handle.abort();
        let _ = handle.await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["AccessKeyId"], "AKIATEST");
        assert_eq!(body["SecretAccessKey"], "SECRET");
        assert_eq!(body["Token"], "TOKEN");
    }

    #[tokio::test]
    async fn creds_401_wrong_token() {
        let state = test_state();
        let bad = format!("{}-wrong", state.bearer_token);
        let (status, body) = creds_request_with(Some(&bad)).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, Value::Null);
    }

    #[tokio::test]
    async fn creds_401_missing_token() {
        let (status, body) = creds_request_with(None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, Value::Null);
    }
}
