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
use serde_json::{json, Map, Value};
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use uuid::Uuid;

struct AppState {
    bearer_token: String,
    creds_provider: Arc<dyn ProvideCredentials>,
    region: String,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    profile: Option<String>,
}

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
        .with_state(state);

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).into_future().await {
            eprintln!("harnx-aws-creds: server error: {err}");
        }
    });

    Ok(port)
}

async fn run_hook_loop(state: &AppState, port: u16) -> Result<()> {
    let stdin = stdin();
    let stdout = stdout();
    run_hook_loop_io(state, port, stdin, stdout).await
}

/// Process a single JSONL hook line and return the response Value to write,
/// or None if the line should be silently skipped (malformed JSON, missing id).
fn handle_hook_line(line: &str, state: &AppState, port: u16) -> Option<Value> {
    // Parse JSON — if malformed, log and skip (don't kill the process).
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("harnx-aws-creds: ignoring malformed JSON line: {err}");
            return None;
        }
    };

    // Extract id — required to produce a valid response. If missing, skip.
    let id = match request.get("id").and_then(Value::as_str) {
        Some(id) => id,
        None => {
            eprintln!("harnx-aws-creds: ignoring hook event with missing/non-string id");
            return None;
        }
    };

    let response = match (
        request.get("hook_event_name").and_then(Value::as_str),
        request.get("tool_name").and_then(Value::as_str),
        request.get("tool_input"),
    ) {
        (Some("PreToolUse"), Some("bash_exec" | "bash_spawn"), Some(tool_input)) => {
            match mutate_tool_input(tool_input, state, port) {
                Ok(mutated) => json!({
                    "id": id,
                    "hookSpecificOutput": {
                        "toolInput": mutated,
                    }
                }),
                Err(err) => {
                    // Mutation failed (e.g. tool_input/env not an object) — emit no-op
                    // so the tool call still proceeds without injection.
                    eprintln!("harnx-aws-creds: failed to mutate tool input: {err}");
                    json!({ "id": id })
                }
            }
        }
        _ => json!({ "id": id }),
    };

    Some(response)
}

async fn run_hook_loop_io<R, W>(state: &AppState, port: u16, input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(input).lines();

    while let Some(line) = lines.next_line().await? {
        if let Some(response) = handle_hook_line(&line, state, port) {
            let mut encoded = serde_json::to_vec(&response)?;
            encoded.push(b'\n');
            output.write_all(&encoded).await?;
            output.flush().await?;
        }
    }

    Ok(())
}

fn mutate_tool_input(tool_input: &Value, state: &AppState, port: u16) -> Result<Value> {
    let mut tool_input = tool_input
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("tool_input must be object"))?;

    let env_value = tool_input
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let env = env_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("tool_input.env must be object"))?;

    env.insert(
        "AWS_CONTAINER_CREDENTIALS_FULL_URI".to_string(),
        Value::String(format!("http://127.0.0.1:{port}/creds")),
    );
    env.insert(
        "AWS_CONTAINER_AUTHORIZATION_TOKEN".to_string(),
        Value::String(state.bearer_token.clone()),
    );
    env.insert(
        "AWS_REGION".to_string(),
        Value::String(state.region.clone()),
    );

    Ok(Value::Object(tool_input))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let state = build_app_state(&args).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = start_server(Arc::clone(&state), listener).await?;

    eprintln!("harnx-aws-creds: listening on http://127.0.0.1:{port}/creds");
    eprintln!(
        "harnx-aws-creds: authorization token: {}",
        state.bearer_token
    );

    run_hook_loop(&state, port).await
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

    /// Shared wiring: write `input` into a duplex pipe, run the hook loop, return raw output.
    async fn run_hook_with_io(input: &str, port: u16) -> String {
        let state = test_state();
        let (mut input_writer, input_reader) = tokio::io::duplex(16384);
        input_writer.write_all(input.as_bytes()).await.unwrap();
        drop(input_writer);

        let (output_writer, mut output_reader) = tokio::io::duplex(16384);
        run_hook_loop_io(&state, port, input_reader, output_writer)
            .await
            .unwrap();

        let mut output = String::new();
        output_reader.read_to_string(&mut output).await.unwrap();
        output
    }

    async fn run_hook_once(input: &str, port: u16) -> String {
        run_hook_with_io(input, port).await
    }

    #[tokio::test]
    async fn creds_200_valid_token() {
        let state = test_state();
        let (port, handle) = spawn_test_server(state.clone()).await;

        let (status, body) = read_http_response(port, Some(&state.bearer_token)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["AccessKeyId"], "AKIATEST");
        assert_eq!(body["SecretAccessKey"], "SECRET");
        assert_eq!(body["Token"], "TOKEN");

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn creds_401_wrong_token() {
        let state = test_state();
        let (port, handle) = spawn_test_server(test_state()).await;

        let (status, body) =
            read_http_response(port, Some(&format!("{}-wrong", state.bearer_token))).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, Value::Null);

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn creds_401_missing_token() {
        let (port, handle) = spawn_test_server(test_state()).await;

        let (status, body) = read_http_response(port, None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, Value::Null);

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn hook_injects_env_bash_exec() {
        let output = run_hook_once(
            "{\"id\":\"1\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"bash_exec\",\"tool_input\":{\"command\":\"ls\",\"env\":{\"EXISTING\":\"val\"}}}\n",
            12345,
        )
        .await;

        let response: Value = serde_json::from_str(output.trim()).unwrap();
        let env = &response["hookSpecificOutput"]["toolInput"]["env"];
        assert_eq!(response["id"], "1");
        assert_eq!(env["EXISTING"], "val");
        assert_eq!(
            env["AWS_CONTAINER_CREDENTIALS_FULL_URI"],
            "http://127.0.0.1:12345/creds"
        );
        assert_eq!(env["AWS_CONTAINER_AUTHORIZATION_TOKEN"], "testtoken");
        assert_eq!(env["AWS_REGION"], "us-east-1");
    }

    #[tokio::test]
    async fn hook_injects_env_no_prior_env() {
        let output = run_hook_once(
            "{\"id\":\"1\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"bash_exec\",\"tool_input\":{\"command\":\"ls\"}}\n",
            12345,
        )
        .await;

        let response: Value = serde_json::from_str(output.trim()).unwrap();
        let env = &response["hookSpecificOutput"]["toolInput"]["env"];
        assert_eq!(response["id"], "1");
        assert_eq!(
            env["AWS_CONTAINER_CREDENTIALS_FULL_URI"],
            "http://127.0.0.1:12345/creds"
        );
        assert_eq!(env["AWS_CONTAINER_AUTHORIZATION_TOKEN"], "testtoken");
        assert_eq!(env["AWS_REGION"], "us-east-1");
    }

    #[tokio::test]
    async fn hook_noop_other_tool() {
        let output = run_hook_once(
            "{\"id\":\"1\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"fs_read\",\"tool_input\":{\"path\":\"/tmp/file\"}}\n",
            12345,
        )
        .await;

        let response: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(response, json!({ "id": "1" }));
    }

    #[tokio::test]
    async fn hook_noop_other_event() {
        let output = run_hook_once(
            "{\"id\":\"1\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"bash_exec\",\"tool_input\":{\"command\":\"ls\"}}\n",
            12345,
        )
        .await;

        let response: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(response, json!({ "id": "1" }));
    }

    #[tokio::test]
    async fn hook_aws_vars_overwrite_existing() {
        let output = run_hook_once(
            "{\"id\":\"1\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"bash_exec\",\"tool_input\":{\"command\":\"ls\",\"env\":{\"AWS_REGION\":\"wrong\"}}}\n",
            12345,
        )
        .await;

        let response: Value = serde_json::from_str(output.trim()).unwrap();
        let env = &response["hookSpecificOutput"]["toolInput"]["env"];
        assert_eq!(env["AWS_REGION"], "us-east-1");
        assert_eq!(env["AWS_CONTAINER_AUTHORIZATION_TOKEN"], "testtoken");
    }

    // Helper: send multiple lines, collect all output lines as parsed Values
    async fn run_hook_multi(inputs: &[&str], port: u16) -> Vec<Value> {
        let output = run_hook_with_io(&inputs.join(""), port).await;
        output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn hook_malformed_json_skipped_continues_loop() {
        // Malformed line followed by valid line — loop must survive and process the valid one.
        let responses = run_hook_multi(
            &[
                "this is not json\n",
                "{\"id\":\"2\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"fs_read\",\"tool_input\":{}}\n",
            ],
            12345,
        )
        .await;

        // Malformed line produces no output; valid no-op produces one response.
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], "2");
    }

    #[tokio::test]
    async fn hook_missing_id_skipped_continues_loop() {
        // Line with no id field, then valid line — loop must survive.
        let responses = run_hook_multi(
            &[
                "{\"hook_event_name\":\"SessionStart\"}\n",
                "{\"id\":\"3\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"bash_exec\"}\n",
            ],
            12345,
        )
        .await;

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], "3");
    }

    #[tokio::test]
    async fn hook_non_object_tool_input_emits_noop() {
        // tool_input is not an object — mutate_tool_input fails, hook emits no-op for the event.
        let output = run_hook_once(
            "{\"id\":\"4\",\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"bash_exec\",\"tool_input\":\"not-an-object\"}\n",
            12345,
        )
        .await;

        let response: Value = serde_json::from_str(output.trim()).unwrap();
        // Falls back to no-op — no hookSpecificOutput
        assert_eq!(response["id"], "4");
        assert!(response.get("hookSpecificOutput").is_none());
    }
}
