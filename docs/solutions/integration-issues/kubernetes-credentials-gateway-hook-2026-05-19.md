---
title: "Kubernetes credentials gateway via persistent PreToolUse hook"
date: 2026-05-19
category: integration-issues
problem_type: integration_issue
component: harnx-k8s-creds
root_cause: "Sandboxed bash processes cannot access host KUBECONFIG or exec authenticators; long-lived tokens leak into sandbox"
resolution_type: code_fix
severity: high
tags:
  - kubernetes
  - k8s
  - credentials
  - hooks
  - persistent-hooks
  - sandboxing
  - kubeconfig
  - exec-plugin
plan_ref: harnx-k8s-creds
---

## Problem

Sandboxed bash processes had no access to Kubernetes clusters without exposing sensitive host configuration. The sandbox strips environment variables and doesn't mount `~/.kube`. Users had to pass `KUBECONFIG` or long-lived tokens inline — insecure and impractical for exec-based auth (EKS, GKE).

## Symptoms

- `kubectl` commands in sandboxed `bash_exec` failed with `Unable to connect to the server: dial tcp: missing address`
- AWS EKS contexts using `aws-iam-authenticator` exec plugin could not authenticate
- GKE contexts using `gke-gcloud-auth-plugin` exec plugin could not authenticate
- Users could not leverage SSO, IAM roles, or short-lived token rotation inside sandbox
- No integration path for standard Kubernetes authentication flows into sandbox

## Investigation Steps

1. Reviewed `harnx-aws-creds` architecture — same credential relay pattern applies
2. Evaluated Kubernetes authentication: static tokens, token files, exec credential plugins
3. Discovered `kube::Config.auth_info` exposes resolved token after loading context
4. Tested synthetic kubeconfig with `exec` block calling `curl` — works with standard `kubectl`
5. Identified token extraction paths: `auth_info.token`, `auth_info.token_file`, `auth_info.exec`

## Root Cause

Kubernetes authentication has no container credential provider protocol equivalent to AWS. The sandbox environment cannot access:
- Host `KUBECONFIG` file (not mounted)
- Exec authenticator binaries (host-side only)
- Long-lived static tokens (security risk)
- AWS/GCP credentials used by exec plugins

Hook mutation provides an injection point, but requires a token relay mechanism.

## Solution

Implemented `harnx-k8s-creds` persistent hook following `harnx-aws-creds` architecture:

### 1. Local HTTP Token Server

Starts HTTP server on `127.0.0.1:0` serving tokens via `/token/<context>` endpoint:

```rust
async fn token_handler(
    State(state): State<Arc<AppState>>,
    Path(context_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Validate bearer token
    let actual = headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok());
    let expected = format!("Bearer {}", state.bearer_token);
    if actual != Some(expected.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Resolve token from host kubeconfig
    match resolve_token(entry) {
        Ok(cached) => Json(ExecCredential {
            api_version: "client.authentication.k8s.io/v1".into(),
            kind: "ExecCredential".into(),
            status: ExecCredentialStatus {
                token: cached.token,
                expiration_timestamp: cached.expires_at.map(|t| t.to_rfc3339()),
            },
        }).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}
```

### 2. Token Resolution from Host Kubeconfig

Three auth sources, in priority order:

```rust
fn resolve_token(entry: &ContextEntry) -> Result<CachedToken> {
    let auth_info = &entry.config.auth_info;
    
    // 1. Static token in kubeconfig
    if let Some(token) = auth_info.token.as_ref() {
        return Ok(CachedToken {
            token: token.expose_secret().to_string(),
            expires_at: None,
        });
    }
    
    // 2. Token file path
    if let Some(token_file) = auth_info.token_file.as_ref() {
        return Ok(CachedToken {
            token: std::fs::read_to_string(token_file)?.trim().to_string(),
            expires_at: None,
        });
    }
    
    // 3. Exec credential plugin (aws-iam-authenticator, gke-gcloud-auth-plugin)
    if let Some(exec) = auth_info.exec.as_ref() {
        let exec_credential = run_exec_plugin(exec)?;
        return Ok(CachedToken {
            token: exec_credential.status.token,
            expires_at: parse_expiration(&exec_credential.status.expiration_timestamp)?,
        });
    }
    
    bail!("no supported auth type for context")
}
```

### 3. Exec Plugin Invocation

Exec plugins run on host with `KUBERNETES_EXEC_INFO` env var:

```rust
fn run_exec_plugin(exec: &ExecConfig) -> Result<ExecCredential> {
    let exec_info = json!({
        "apiVersion": exec.api_version,
        "kind": "ExecCredential",
        "spec": { "interactive": false }
    }).to_string();

    let mut cmd = Command::new(exec.command.as_deref().unwrap_or(""));
    cmd.args(exec.args.as_deref().unwrap_or(&[]));
    cmd.env("KUBERNETES_EXEC_INFO", exec_info);
    
    let output = cmd.output()?;
    serde_json::from_slice(&output.stdout)
}
```

### 4. Synthetic Kubeconfig with curl Exec Block

Write temporary kubeconfig that uses `curl` to fetch tokens:

```rust
fn write_synthetic_kubeconfig(state: &AppState, port: u16) -> Result<TempPath> {
    let users = state.contexts.iter().map(|ctx| {
        json!({
            "name": ctx.name,
            "user": {
                "exec": {
                    "apiVersion": "client.authentication.k8s.io/v1",
                    "command": "curl",
                    "args": [
                        "--silent", "--fail",
                        "--header", format!("Authorization: Bearer {}", state.bearer_token),
                        format!("http://127.0.0.1:{port}/token/{}", ctx.name)
                    ],
                    "interactiveMode": "Never"
                }
            }
        })
    });
    
    // Write to temp file, return TempPath for lifecycle management
    let mut file = tempfile::NamedTempFile::new()?;
    serde_yaml::to_writer(&mut file, &config)?;
    Ok(file.into_temp_path())
}
```

### 5. Persistent PreToolUse Hook

Injects synthetic `KUBECONFIG` into `bash_exec`/`bash_spawn`:

```rust
fn handle_hook_line(line: &str, kubeconfig_path: KubeconfigPath<'_>) -> Option<Value> {
    let input: Value = serde_json::from_str(line).ok()?;
    let id = input.get("id")?.clone();
    let hook_event_name = input.get("hook_event_name").and_then(Value::as_str);
    let tool_name = input.get("tool_name").and_then(Value::as_str);

    if hook_event_name == Some("PreToolUse")
        && matches!(tool_name, Some("bash_exec") | Some("bash_spawn"))
    {
        if let Some(tool_input) = input.get("tool_input") {
            if let Ok(mutated) = mutate_tool_input(tool_input, kubeconfig_path) {
                return Some(json!({
                    "id": id,
                    "hookSpecificOutput": { "toolInput": mutated }
                }));
            }
        }
    }
    Some(json!({ "id": id }))
}
```

### 6. k8s-openapi Direct Dependency

Binary crate must depend on `k8s-openapi` directly for feature activation:

```toml
# crates/harnx-k8s-creds/Cargo.toml
[dependencies]
kube = { workspace = true }
k8s-openapi = { workspace = true }  # Required for v1_30 feature
```

Build script checks direct dependency for feature flag activation.

### 7. secrecy::ExposeSecret Trait

`auth_info.token` is `SecretString`, requires trait import:

```rust
use secrecy::ExposeSecret;

// Access wrapped value
let token = auth_info.token.as_ref().unwrap().expose_secret().to_string();
```

### 8. ExecConfig Test Construction

`ExecConfig` lacks `Default` impl, use `serde_json::from_value`:

```rust
fn make_echo_exec(json_output: &str) -> ExecConfig {
    serde_json::from_value(json!({
        "command": "echo",
        "args": [json_output],
        "apiVersion": "client.authentication.k8s.io/v1",
        "provideClusterInfo": false
    })).unwrap()
}
```

## Why This Works

1. **Standard kubectl exec auth**: Synthetic kubeconfig uses standard k8s exec credential protocol — no kubectl patches needed
2. **Loopback-only binding**: `127.0.0.1:0` limits exposure to local processes
3. **Per-session bearer token**: UUID token gates `/token/<context>` endpoint
4. **Host exec plugins**: `aws-iam-authenticator`, `gke-gcloud-auth-plugin` run on host with host credentials
5. **Token caching**: 60-second skew prevents stale token use, reduces exec plugin calls
6. **No host kubeconfig exposure**: Sandbox receives syntheticConfig with loopback URL only
7. **Hook mutation path**: Injected env vars pass through sandbox's authorized mechanism

## Prevention Strategies

**Test Cases:**
- `token_200_valid_request`: Valid bearer returns ExecCredential JSON
- `token_401_wrong_token`: Wrong token returns 401
- `token_401_missing_token`: Missing auth header returns 401
- `token_404_unknown_context`: Unknown context returns 404
- `hook_injects_kubeconfig_bash_exec`: PreToolUse injection for bash_exec
- `hook_injects_kubeconfig_bash_spawn`: PreToolUse injection for bash_spawn
- `hook_noop_other_tool`: No mutation for non-bash tools
- `resolve_token_static_token`: Static token from `auth_info.token`
- `resolve_token_token_file`: Token read from file path
- `resolve_token_exec_plugin_path`: Token from exec plugin output
- `run_exec_plugin_success`: Echo-based exec plugin simulation
- `run_exec_plugin_failure_returns_error`: Failed exec returns error

**Best Practices:**
- Use `match`+`continue` in persistent hook loops, never `?`
- Emit no-op response for mutation failures so tool call proceeds
- Test exec plugins with `echo` command for deterministic output
- Use `tempfile::NamedTempFile::new()?.into_temp_path()` for kubeconfig lifecycle
- Import `secrecy::ExposeSecret` to access wrapped `SecretString` values
- Add `k8s-openapi` as direct dep in binary crates for feature activation

**Code Review Checklist:**
- [ ] Hook loop uses `match`/`continue` for parse errors
- [ ] Mutation failures emit no-op, not error
- [ ] Response format is flattened (top-level `hookSpecificOutput`)
- [ ] HTTP server binds to `127.0.0.1:0` (loopback only)
- [ ] Token endpoint validates bearer before serving
- [ ] Exec plugin env vars use `HashMap.get("name")`/`get("value")`
- [ ] Synthetic kubeconfig uses `curl --fail` for error propagation
- [ ] `k8s-openapi` direct dependency with workspace feature

## Related Issues

- **GitHub Issue:** [#592 — Kubernetes credentials/config gateway](https://github.com/dobesv/harnx/issues/592)
- **Related Solution:** [aws-credentials-hook-persistent-2026-05-15.md](./aws-credentials-hook-persistent-2026-05-15.md) — AWS credential relay architecture
- **Related Solution:** [aws-credential-chain-caching-2026-05-15.md](./aws-credential-chain-caching-2026-05-15.md) — AWS credential caching patterns
- **Related Solution:** [hooks-mutation-implementation-2026-05-14.md](../logic-errors/hooks-mutation-implementation-2026-05-14.md) — Hook mutation mechanism
- **Related Solution:** [per-call-env-param-bash-mcp-2026-05-13.md](../api-design/per-call-env-param-bash-mcp-2026-05-13.md) — Env injection through bash tools
- **Related Solution:** [environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Why kubeconfig is inaccessible
