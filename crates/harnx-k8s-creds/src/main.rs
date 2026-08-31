use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{Path, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use kube::config::{ExecConfig, KubeConfigOptions, Kubeconfig};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path as FsPath, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempPath;
use urlencoding::encode as pct_encode;

/// Newtype for the path of the synthetic kubeconfig file.
/// Avoids passing a raw `&str` through every hook-loop function.
#[derive(Clone, Copy)]
struct KubeconfigPath<'a>(&'a str);
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use uuid::Uuid;

struct AppState {
    bearer_token: String,
    contexts: Vec<ContextEntry>,
}

struct ContextEntry {
    name: String,
    config: kube::Config,
    cached: Mutex<Option<CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExecCredential {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    status: ExecCredentialStatus,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExecCredentialStatus {
    token: String,
    #[serde(
        rename = "expirationTimestamp",
        skip_serializing_if = "Option::is_none"
    )]
    expiration_timestamp: Option<String>,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, num_args = 1..)]
    context: Vec<String>,
    #[arg(long)]
    kubeconfig: Option<PathBuf>,

    #[command(flatten)]
    metrics: harnx_metrics::MetricsFlags,
    #[command(flatten)]
    healthz: harnx_healthz::HealthzFlags,
}

async fn build_app_state(args: &Args) -> Result<Arc<AppState>> {
    if args.context.is_empty() {
        bail!("at least one --context is required");
    }

    let kubeconfig_path = args
        .kubeconfig
        .clone()
        .or_else(|| std::env::var("KUBECONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(default_kubeconfig_path);

    let kubeconfig = Kubeconfig::read_from(&kubeconfig_path)
        .with_context(|| format!("failed to read kubeconfig: {}", kubeconfig_path.display()))?;

    let mut contexts = Vec::with_capacity(args.context.len());
    for name in &args.context {
        let config = kube::Config::from_custom_kubeconfig(
            kubeconfig.clone(),
            &KubeConfigOptions {
                context: Some(name.clone()),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("failed to load kube config for context {name}"))?;

        contexts.push(ContextEntry {
            name: name.clone(),
            config,
            cached: Mutex::new(None),
        });
    }

    Ok(Arc::new(AppState {
        bearer_token: Uuid::new_v4().to_string(),
        contexts,
    }))
}

fn default_kubeconfig_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
        .join(".kube/config")
}

fn resolve_token(entry: &ContextEntry) -> Result<CachedToken> {
    let mut cached = entry
        .cached
        .lock()
        .map_err(|_| anyhow!("token cache mutex poisoned"))?;

    if let Some(token) = cached.as_ref() {
        let still_valid = token
            .expires_at
            .map(|time| time > Utc::now() + Duration::seconds(60))
            .unwrap_or(true);
        if still_valid {
            return Ok(token.clone());
        }
    }

    let auth_info = &entry.config.auth_info;
    let resolved = if let Some(token) = auth_info.token.as_ref() {
        CachedToken {
            token: token.expose_secret().to_string(),
            expires_at: None,
        }
    } else if let Some(token_file) = auth_info.token_file.as_ref() {
        CachedToken {
            token: std::fs::read_to_string(token_file)
                .with_context(|| format!("failed to read token file: {token_file}"))?
                .trim()
                .to_string(),
            expires_at: None,
        }
    } else if let Some(exec) = auth_info.exec.as_ref() {
        let exec_credential = run_exec_plugin(exec)?;
        CachedToken {
            token: exec_credential.status.token,
            expires_at: exec_credential
                .status
                .expiration_timestamp
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|time| time.with_timezone(&Utc)),
        }
    } else {
        bail!("no supported auth type for context");
    };

    *cached = Some(resolved.clone());
    Ok(resolved)
}

fn run_exec_plugin(exec: &ExecConfig) -> Result<ExecCredential> {
    let command = exec.command.as_deref().unwrap_or("");
    if command.is_empty() {
        bail!("exec plugin missing command");
    }

    let exec_info = json!({
        "apiVersion": exec.api_version,
        "kind": "ExecCredential",
        "spec": { "interactive": false }
    })
    .to_string();

    let mut cmd = Command::new(command);
    cmd.args(exec.args.as_deref().unwrap_or(&[]));
    cmd.env("KUBERNETES_EXEC_INFO", exec_info);

    for env_map in exec.env.as_deref().unwrap_or(&[]) {
        if let (Some(name), Some(value)) = (env_map.get("name"), env_map.get("value")) {
            cmd.env(name, value);
        }
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to run exec plugin: {command}"))?;
    if !output.status.success() {
        bail!(
            "exec plugin failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Trim trailing whitespace/CRLF before parsing (echo on Windows emits \r\n).
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).context("failed to parse exec plugin output")
}

async fn token_handler(
    State(state): State<Arc<AppState>>,
    Path(context_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actual = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let expected = format!("Bearer {}", state.bearer_token);
    if actual != Some(expected.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let Some(entry) = state
        .contexts
        .iter()
        .find(|entry| entry.name == context_name)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match resolve_token(entry) {
        Ok(cached) => Json(ExecCredential {
            api_version: "client.authentication.k8s.io/v1".into(),
            kind: "ExecCredential".into(),
            status: ExecCredentialStatus {
                token: cached.token,
                expiration_timestamp: cached.expires_at.map(|time| time.to_rfc3339()),
            },
        })
        .into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn start_server(state: Arc<AppState>, listener: TcpListener) -> Result<u16> {
    let port = listener.local_addr()?.port();
    let router = Router::new()
        .route("/token/{context}", get(token_handler))
        .layer(axum::middleware::from_fn(
            harnx_metrics::http_metrics_middleware,
        ))
        .with_state(state);

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            eprintln!("harnx-k8s-creds: server error: {err}");
        }
    });

    Ok(port)
}

fn root_cert_to_ca_data(certs: &[Vec<u8>]) -> String {
    let pem = certs
        .iter()
        .map(|der| {
            format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                STANDARD.encode(der)
            )
        })
        .collect::<String>();
    STANDARD.encode(pem.as_bytes())
}

fn build_cluster_entry(ctx: &ContextEntry) -> Value {
    let mut cluster = Map::new();
    cluster.insert(
        "server".to_string(),
        Value::String(ctx.config.cluster_url.to_string()),
    );
    if let Some(root_cert) = ctx.config.root_cert.as_ref() {
        cluster.insert(
            "certificate-authority-data".to_string(),
            Value::String(root_cert_to_ca_data(root_cert)),
        );
    }
    json!({ "name": ctx.name, "cluster": Value::Object(cluster) })
}

fn build_user_entry(ctx: &ContextEntry, state: &AppState, port: u16) -> Value {
    json!({
        "name": ctx.name,
        "user": {
            "exec": {
                "apiVersion": "client.authentication.k8s.io/v1",
                "command": "curl",
                "args": [
                    "--silent", "--fail", "--header",
                    format!("Authorization: Bearer {}", state.bearer_token),
                    format!("http://127.0.0.1:{port}/token/{}", pct_encode(&ctx.name))
                ],
                "interactiveMode": "Never"
            }
        }
    })
}

fn build_context_entry(ctx: &ContextEntry) -> Value {
    json!({ "name": ctx.name, "context": { "cluster": ctx.name, "user": ctx.name } })
}

fn write_synthetic_kubeconfig(state: &AppState, port: u16) -> Result<TempPath> {
    if state.contexts.is_empty() {
        bail!("no contexts configured");
    }
    let clusters: Vec<_> = state.contexts.iter().map(build_cluster_entry).collect();
    let users: Vec<_> = state
        .contexts
        .iter()
        .map(|ctx| build_user_entry(ctx, state, port))
        .collect();
    let contexts: Vec<_> = state.contexts.iter().map(build_context_entry).collect();
    let config = json!({
        "apiVersion": "v1", "kind": "Config",
        "clusters": clusters, "users": users, "contexts": contexts,
        "current-context": state.contexts[0].name,
    });
    let mut file = tempfile::NamedTempFile::new().context("failed to create temp kubeconfig")?;
    serde_yaml::to_writer(&mut file, &config).context("failed to write synthetic kubeconfig")?;
    Ok(file.into_temp_path())
}

async fn run_hook_loop(kubeconfig_path: &FsPath) -> Result<()> {
    let path_str = kubeconfig_path
        .to_str()
        .ok_or_else(|| anyhow!("kubeconfig path is not valid UTF-8"))?;
    run_hook_loop_io(KubeconfigPath(path_str), stdin(), stdout()).await
}

fn handle_hook_line(line: &str, kubeconfig_path: KubeconfigPath<'_>) -> Option<Value> {
    let input: Value = serde_json::from_str(line).ok()?;
    let id = input.get("id")?.clone();
    let hook_event_name = input.get("hook_event_name").and_then(Value::as_str);

    // Respond to any PreToolUse event with a tool_input — tool name filtering
    // is the caller's responsibility (configured via the hook's matcher field).
    if hook_event_name == Some("PreToolUse") {
        if let Some(tool_input) = input.get("tool_input") {
            if let Ok(tool_input) = mutate_tool_input(tool_input, kubeconfig_path) {
                return Some(json!({
                    "id": id,
                    "hookSpecificOutput": {
                        "toolInput": tool_input
                    }
                }));
            }
        }
    }

    Some(json!({ "id": id }))
}

async fn run_hook_loop_io<R, W>(
    kubeconfig_path: KubeconfigPath<'_>,
    input: R,
    mut output: W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if let Some(response) = handle_hook_line(&line, kubeconfig_path) {
            let mut encoded = serde_json::to_vec(&response)?;
            encoded.push(b'\n');
            output.write_all(&encoded).await?;
            output.flush().await?;
        }
    }
    Ok(())
}

fn mutate_tool_input(tool_input: &Value, kubeconfig_path: KubeconfigPath<'_>) -> Result<Value> {
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
        "KUBECONFIG".to_string(),
        Value::String(kubeconfig_path.0.to_string()),
    );

    Ok(Value::Object(tool_input))
}

#[tokio::main]
async fn main() -> Result<()> {
    let telemetry = harnx_telemetry::init_telemetry("harnx-k8s-creds")?;

    let result = run().await;
    telemetry.shutdown().await;
    result
}

async fn run() -> Result<()> {
    let args = Args::parse();
    harnx_metrics::init(&args.metrics)?;
    let readiness = harnx_healthz::init(&args.healthz).await?;
    let state = build_app_state(&args).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = start_server(Arc::clone(&state), listener).await?;
    if let Some(r) = &readiness {
        r.ready();
    }
    let temp_path = write_synthetic_kubeconfig(&state, port)?;

    eprintln!(
        "harnx-k8s-creds: synthetic kubeconfig: {}",
        temp_path.to_string_lossy()
    );

    run_hook_loop(temp_path.as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::{routing::get, Router};
    use serde_json::Value;
    use tempfile::NamedTempFile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn run_hook_with_io(input: &str, kubeconfig_path: KubeconfigPath<'_>) -> String {
        let (mut input_writer, input_reader) = tokio::io::duplex(16384);
        input_writer.write_all(input.as_bytes()).await.unwrap();
        drop(input_writer);

        let (output_writer, mut output_reader) = tokio::io::duplex(16384);
        run_hook_loop_io(kubeconfig_path, input_reader, output_writer)
            .await
            .unwrap();

        let mut output = String::new();
        output_reader.read_to_string(&mut output).await.unwrap();
        output
    }

    async fn run_hook_once(event: &Value, kubeconfig_path: KubeconfigPath<'_>) -> String {
        let line = format!("{event}\n");
        run_hook_with_io(&line, kubeconfig_path).await
    }

    async fn run_hook_multi(inputs: &[&str], kubeconfig_path: KubeconfigPath<'_>) -> Vec<Value> {
        let output = run_hook_with_io(&inputs.join(""), kubeconfig_path).await;
        output
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Assert that `event` produces a single-field `{"id": expected_id}` noop with no hookSpecificOutput.
    async fn assert_noop(event: &Value, expected_id: u32) {
        let output = run_hook_once(event, KubeconfigPath("/synthetic/path")).await;
        let response: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(response["id"], expected_id.to_string());
        assert!(response.get("hookSpecificOutput").is_none());
    }

    /// Assert that `bad_line` is skipped and `good_line` produces exactly one noop with `expected_id`.
    async fn assert_skipped_continues(bad_line: &str, good_line: &str, expected_id: u32) {
        let responses =
            run_hook_multi(&[bad_line, good_line], KubeconfigPath("/synthetic/path")).await;
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], expected_id.to_string());
    }

    async fn hook_injected_env(tool_input: Value) -> Value {
        let event = json!({
            "id": "1", "hook_event_name": "PreToolUse",
            "tool_name": "bash_exec", "tool_input": tool_input
        });
        let output = run_hook_once(&event, KubeconfigPath("/synthetic/path")).await;
        let response: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(response["id"], "1");
        response["hookSpecificOutput"]["toolInput"]["env"].clone()
    }

    fn test_state() -> Arc<AppState> {
        let uri = "https://127.0.0.1:6443".parse().unwrap();
        let mut config = kube::Config::new(uri);
        config.root_cert = Some(vec![vec![1, 2, 3, 4]]);

        Arc::new(AppState {
            bearer_token: "testtoken".to_string(),
            contexts: vec![ContextEntry {
                name: "test-context".to_string(),
                config,
                cached: Mutex::new(Some(CachedToken {
                    token: "test-k8s-token".to_string(),
                    expires_at: None,
                })),
            }],
        })
    }

    async fn spawn_test_server(state: Arc<AppState>) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new()
            .route("/token/{context}", get(token_handler))
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (port, handle)
    }

    async fn token_request_with(
        port: u16,
        context: &str,
        auth: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let auth_header = auth
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET /token/{context} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth_header}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status_code = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let body = body.trim();
        let body = body.strip_suffix("0").map(str::trim).unwrap_or(body);
        let body = if let Some((_, chunk_body)) = body.split_once("\r\n") {
            chunk_body.trim()
        } else {
            body
        };
        let json = serde_json::from_str(body).unwrap_or(Value::Null);

        (StatusCode::from_u16(status_code).unwrap(), json)
    }

    #[tokio::test]
    async fn hook_non_pretooluse_event_emits_noop() {
        // PostToolUse events are always ignored regardless of tool name.
        assert_noop(
            &json!({"id":"1","hook_event_name":"PostToolUse","tool_name":"bash_exec","tool_input":{"command":"ls"}}),
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn hook_bash_exec_injects_kubeconfig() {
        let env = hook_injected_env(json!({"command":"ls","env":{"EXISTING":"val"}})).await;
        assert_eq!(env["EXISTING"], "val");
        assert_eq!(env["KUBECONFIG"], "/synthetic/path");
    }

    #[tokio::test]
    async fn hook_bash_spawn_injects_kubeconfig() {
        let event = json!({"id":"1","hook_event_name":"PreToolUse","tool_name":"bash_spawn","tool_input":{"command":"sleep 1","env":{}}});
        let output = run_hook_once(&event, KubeconfigPath("/synthetic/path")).await;
        let response: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(
            response["hookSpecificOutput"]["toolInput"]["env"]["KUBECONFIG"],
            "/synthetic/path"
        );
    }

    #[tokio::test]
    async fn hook_kubeconfig_overwrites_existing() {
        let env = hook_injected_env(json!({"command":"ls","env":{"KUBECONFIG":"old"}})).await;
        assert_eq!(env["KUBECONFIG"], "/synthetic/path");
    }

    #[tokio::test]
    async fn hook_missing_env_creates_env_with_kubeconfig() {
        let env = hook_injected_env(json!({"command":"ls"})).await;
        assert_eq!(env["KUBECONFIG"], "/synthetic/path");
    }

    #[tokio::test]
    async fn hook_malformed_json_skipped_continues_loop() {
        assert_skipped_continues(
            "this is not json\n",
            "{\"id\":\"2\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"bash_exec\"}\n",
            2,
        )
        .await;
    }

    #[tokio::test]
    async fn hook_missing_id_skipped_continues_loop() {
        assert_skipped_continues(
            "{\"hook_event_name\":\"SessionStart\"}\n",
            "{\"id\":\"3\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"bash_exec\"}\n",
            3,
        )
        .await;
    }

    #[tokio::test]
    async fn hook_non_object_tool_input_emits_noop() {
        assert_noop(
            &json!({"id":"4","hook_event_name":"PreToolUse","tool_name":"bash_exec","tool_input":"not-an-object"}),
            4,
        )
        .await;
    }

    #[test]
    fn synthetic_kubeconfig_has_exec_block() {
        let state = test_state();
        let path = write_synthetic_kubeconfig(&state, 43123).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let yaml: Value = serde_yaml::from_str(&content).unwrap();

        assert_eq!(yaml["users"][0]["user"]["exec"]["command"], "curl");
        let args = yaml["users"][0]["user"]["exec"]["args"].as_array().unwrap();
        assert!(args
            .iter()
            .any(|value| value == "Authorization: Bearer testtoken"));
        assert!(args
            .iter()
            .any(|value| value == "http://127.0.0.1:43123/token/test-context"));
        assert_eq!(
            yaml["clusters"][0]["cluster"]["certificate-authority-data"],
            root_cert_to_ca_data(&[vec![1, 2, 3, 4]])
        );
    }

    #[tokio::test]
    async fn token_200_valid_request() {
        let state = test_state();
        let token = format!("Bearer {}", state.bearer_token);
        let (port, handle) = spawn_test_server(state).await;
        let (status, body) = token_request_with(port, "test-context", Some(&token)).await;
        handle.abort();
        let _ = handle.await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["kind"], "ExecCredential");
        assert_eq!(body["status"]["token"], "test-k8s-token");
    }

    #[tokio::test]
    async fn token_401_wrong_token() {
        let (port, handle) = spawn_test_server(test_state()).await;
        let (status, _) = token_request_with(port, "test-context", Some("Bearer wrong")).await;
        handle.abort();
        let _ = handle.await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_401_missing_token() {
        let (port, handle) = spawn_test_server(test_state()).await;
        let (status, _) = token_request_with(port, "test-context", None).await;
        handle.abort();
        let _ = handle.await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_404_unknown_context() {
        let state = test_state();
        let token = format!("Bearer {}", state.bearer_token);
        let (port, handle) = spawn_test_server(state).await;
        let (status, _) = token_request_with(port, "missing-context", Some(&token)).await;
        handle.abort();
        let _ = handle.await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn resolve_token_static_token() {
        let entry = ContextEntry {
            name: "ctx".to_string(),
            config: kube::Config::new("https://127.0.0.1:6443".parse().unwrap()),
            cached: Mutex::new(Some(CachedToken {
                token: "mytoken".to_string(),
                expires_at: None,
            })),
        };

        let result = resolve_token(&entry).unwrap();
        assert_eq!(result.token, "mytoken");
    }

    #[test]
    fn resolve_token_cache_hit_returns_cached() {
        let entry = ContextEntry {
            name: "ctx".to_string(),
            config: kube::Config::new("https://127.0.0.1:6443".parse().unwrap()),
            cached: Mutex::new(Some(CachedToken {
                token: "cached-token".to_string(),
                expires_at: None,
            })),
        };

        let result = resolve_token(&entry).unwrap();
        assert_eq!(result.token, "cached-token");
        assert_eq!(
            entry.cached.lock().unwrap().as_ref().unwrap().token,
            "cached-token"
        );
    }

    #[test]
    fn resolve_token_token_file() {
        use std::io::Write;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "filetoken").unwrap();
        let mut config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
        config.auth_info.token_file = Some(file.path().to_string_lossy().to_string());

        let entry = ContextEntry {
            name: "ctx".to_string(),
            config,
            cached: Mutex::new(None),
        };

        let result = resolve_token(&entry).unwrap();
        assert_eq!(result.token, "filetoken");
    }

    #[test]
    fn resolve_token_no_auth_returns_error() {
        let entry = ContextEntry {
            name: "ctx".to_string(),
            config: kube::Config::new("https://127.0.0.1:6443".parse().unwrap()),
            cached: Mutex::new(None),
        };

        let err = resolve_token(&entry).err().unwrap();
        assert!(err.to_string().contains("no supported auth type"));
    }

    // exec plugin tests spawn external processes and are Unix-only:
    // `echo` is a shell built-in on Windows with no standalone binary,
    // and exec credential plugins (aws-iam-authenticator, gke-gcloud-auth-plugin)
    // are not supported on Windows anyway.
    #[cfg(not(windows))]
    mod exec_plugin_tests {
        use super::*;

        fn echo_exec(json_output: &str) -> ExecConfig {
            serde_json::from_value(json!({
                "command": "echo",
                "args": [json_output],
                "apiVersion": "client.authentication.k8s.io/v1",
                "provideClusterInfo": false
            }))
            .unwrap()
        }

        #[test]
        fn run_exec_plugin_success() {
            let exec = echo_exec(
                r#"{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"exec-token"}}"#,
            );
            let result = run_exec_plugin(&exec).unwrap();
            assert_eq!(result.status.token, "exec-token");
        }

        #[test]
        fn run_exec_plugin_failure_returns_error() {
            let exec: ExecConfig =
                serde_json::from_value(json!({"command": "false", "provideClusterInfo": false}))
                    .unwrap();
            assert!(run_exec_plugin(&exec).is_err());
        }

        #[test]
        fn resolve_token_exec_plugin_path() {
            let mut config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
            config.auth_info.exec = Some(echo_exec(
                r#"{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"exec-resolved-token"}}"#,
            ));

            let entry = ContextEntry {
                name: "ctx".to_string(),
                config,
                cached: Mutex::new(None),
            };

            let result = resolve_token(&entry).unwrap();
            assert_eq!(result.token, "exec-resolved-token");
        }
    }
}
