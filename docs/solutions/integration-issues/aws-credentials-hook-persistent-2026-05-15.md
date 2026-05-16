---
title: "AWS credentials via persistent PreToolUse hook"
date: 2026-05-15
category: integration-issues
problem_type: integration_issue
component: harnx-aws-creds
root_cause: "Sandboxed bash processes cannot read ~/.aws; AWS SDK env vars are stripped by default sanitization"
resolution_type: code_fix
severity: high
tags:
  - aws
  - credentials
  - hooks
  - persistent-hooks
  - sandboxing
  - container-credential-provider
plan_ref: harnx-530-aws-creds-hook
---

## Problem

Sandboxed bash processes had no access to AWS credentials. The sandbox strips environment variables like `AWS_ACCESS_KEY_ID` by default (not in allowlist), and `~/.aws` is not mounted. Users could not run AWS CLI or SDKs inside sandboxed commands without manually passing credentials.

## Symptoms

- `aws sts get-caller-identity` in sandboxed `bash_exec` returned `NoCredentialProviders` error
- AWS SDKs inside sandboxed processes failed to locate credentials
- Users had to manually inline credentials via `AWS_ACCESS_KEY_ID=... aws ...` (insecure)
- No integration path for SSO, IAM roles, or instance profiles into sandbox

## Investigation Steps

1. Confirmed sandbox environment sanitization strips AWS env vars (see `security-issues/environment-sanitization-bash-sandbox-2026-04-29.md`)
2. Discovered AWS SDKs support [container credential provider protocol](https://docs.aws.amazon.com/sdkref/latest/guide/feature-container-credentials.html) via `AWS_CONTAINER_CREDENTIALS_FULL_URI` + `AWS_CONTAINER_AUTHORIZATION_TOKEN`
3. Evaluated hook-based injection: `PreToolUse` hooks can mutate `tool_input.env` (see `logic-errors/hooks-mutation-implementation-2026-05-14.md`)
4. Tested `per-call env param` (see `api-design/per-call-env-param-bash-mcp-2026-05-13.md`) — confirmed injected env vars reach sandboxed process

## Root Cause

The sandbox's environment sanitization intentionally strips sensitive AWS credential env vars. This is correct security posture, but left no authorized path for AWS credential delivery into sandboxed processes. Hook mutation provides an injection point, but requires a credential source and delivery mechanism.

## Solution

Implemented `harnx-aws-creds` persistent hook:

### 1. AWS Container Credential Provider Protocol

Hook starts an HTTP server on `127.0.0.1:0` (loopback, random port) that serves credentials:

```rust
// GET /creds handler validates bearer token, returns AWS credential JSON
match state.creds_provider.provide_credentials().await {
    Ok(creds) => Json(CredsResponse {
        access_key_id: creds.access_key_id().to_string(),
        secret_access_key: creds.secret_access_key().to_string(),
        token: creds.session_token().map(String::from),
        expiration: creds.expiry().map(|e| format_rfc3339(e)),
    }).into_response(),
    Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
}
```

AWS SDKs automatically discover credentials when `AWS_CONTAINER_CREDENTIALS_FULL_URI` and `AWS_CONTAINER_AUTHORIZATION_TOKEN` are set.

### 2. Persistent PreToolUse Hook

Binary runs as persistent process (JSONL over stdin/stdout). For each `PreToolUse` event:

```rust
// Inject env vars into bash_exec / bash_spawn tool input
env.insert("AWS_CONTAINER_CREDENTIALS_FULL_URI", format!("http://127.0.0.1:{port}/creds"));
env.insert("AWS_CONTAINER_AUTHORIZATION_TOKEN", state.bearer_token.clone());
env.insert("AWS_REGION", state.region.clone());
```

Response format (flattened per `JsonlResponse` serde shape):

```json
{"id":"...","hookSpecificOutput":{"toolInput":{"command":"...","env":{...}}}}
```

### 3. Credential Provider Setup

Uses same pattern as `integration-issues/aws-credential-chain-caching-2026-05-15.md`:

```rust
let loader = aws_config::defaults(BehaviorVersion::latest());
if let Some(profile) = &args.profile {
    loader.profile_name(profile);
}
let sdk_config = loader.load().await;
let provider: Arc<dyn ProvideCredentials> = Arc::new(
    sdk_config.credentials_provider().unwrap()
);
```

### 4. Resilient Hook Loop Pattern

Critical: persistent hooks must survive malformed input. Use `match`+`continue`, not `?`:

```rust
// WRONG: ? operator kills process on malformed JSON
let request: Value = serde_json::from_str(&line)?;  // exits on bad input

// CORRECT: log and continue
let request: Value = match serde_json::from_str(&line) {
    Ok(v) => v,
    Err(err) => {
        eprintln!("ignoring malformed JSON: {err}");
        continue;
    }
};
```

For mutation failures, emit no-op so tool call proceeds without injection:

```rust
match mutate_tool_input(tool_input, state, port) {
    Ok(mutated) => json!({"id": id, "hookSpecificOutput": {"toolInput": mutated}}),
    Err(err) => {
        eprintln!("mutation failed: {err}");
        json!({"id": id})  // no-op: tool proceeds without AWS env
    }
}
```

### 5. JSONL Response Format

`JsonlResponse` uses `#[serde(flatten)]` on `result: HookResult`, so fields appear at top level:

```json
// NOT nested under "result"
{"id":"1","hookSpecificOutput":{"toolInput":{...}}}

// NOT this
{"id":"1","result":{"hookSpecificOutput":{...}}}
```

### 6. tokio io-std Feature

`tokio::io::stdin()`/`stdout()` require the `io-std` feature per-crate:

```toml
[dependencies]
tokio = { workspace = true, features = ["io-std"] }
```

Workspace feature sets do NOT inherit to member crates.

### 7. SharedCredentialsProvider Coercion

`sdk_config.credentials_provider()` returns `SharedCredentialsProvider`. Wrap with `Arc::new(...)`:

```rust
let provider: Arc<dyn ProvideCredentials> = Arc::new(sdk_config.credentials_provider().unwrap());
```

The concrete type implements `ProvideCredentials`.

### 8. Test Pattern: tokio::io::duplex

For testing async `AsyncRead`/`AsyncWrite` hook loops without subprocesses:

```rust
let (mut input_writer, input_reader) = tokio::io::duplex(16384);
input_writer.write_all(json_line.as_bytes()).await;
drop(input_writer);  // triggers EOF

let (output_writer, mut output_reader) = tokio::io::duplex(16384);
run_hook_loop_io(&state, port, input_reader, output_writer).await.unwrap();

let mut output = String::new();
output_reader.read_to_string(&mut output).await;
```

## Why This Works

1. **Container credential protocol**: AWS SDKs natively support this protocol — no code changes in calling code
2. **Loopback-only binding**: `127.0.0.1:0` limits exposure to local processes
3. **Per-session bearer token**: UUID token gates `/creds` endpoint; printed to stderr for operator visibility
4. **Hook mutation path**: Injected env vars pass through sandbox's authorized `--env` mechanism
5. **Resilient loop**: Malformed input logged and skipped; hook survives for subsequent valid events
6. **No ~/.aws exposure**: Sandbox never sees credential files; credentials fetched on-demand per request

## Prevention Strategies

**Test Cases:**
- `creds_200_valid_token`: Valid bearer returns credential JSON
- `creds_401_wrong_token`: Wrong token returns 401
- `creds_401_missing_token`: Missing auth header returns 401
- `hook_injects_env_bash_exec`: PreToolUse injection for bash_exec
- `hook_injects_env_no_prior_env`: Creates env map if missing
- `hook_noop_other_tool`: No mutation for non-bash tools
- `hook_malformed_json_skipped_continues_loop`: Loop survives bad JSON
- `hook_missing_id_skipped_continues_loop`: Loop survives missing id
- `hook_non_object_tool_input_emits_noop`: Graceful degradation

**Best Practices:**
- Use `match`+`continue` in persistent hook loops, never `?`
- Emit no-op response for mutation failures so tool call proceeds
- Add `io-std` tokio feature explicitly in binary crates using stdin/stdout
- Test error paths: malformed JSON, missing fields, invalid structure
- Validate bearer token on every HTTP request

**Code Review Checklist:**
- [ ] Hook loop uses `match`/`continue` for parse errors
- [ ] Mutation failures emit no-op, not error
- [ ] Response format is flattened (top-level `hookSpecificOutput`)
- [ ] tokio has `io-std` feature
- [ ] HTTP server binds to `127.0.0.1:0` (loopback only)
- [ ] Tests cover malformed input paths

## Related Issues

- **GitHub Issue:** [#530 — AWS credentials provider for sandbox bash processes](https://github.com/dobesv/harnx/issues/530)
- **Related Solution:** [aws-credential-chain-caching-2026-05-15.md](./aws-credential-chain-caching-2026-05-15.md) — Same AWS provider pattern for Bedrock client
- **Related Solution:** [hooks-mutation-implementation-2026-05-14.md](../logic-errors/hooks-mutation-implementation-2026-05-14.md) — Hook mutation mechanism
- **Related Solution:** [per-call-env-param-bash-mcp-2026-05-13.md](../api-design/per-call-env-param-bash-mcp-2026-05-13.md) — Env injection through bash tools
- **Related Solution:** [environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Why AWS vars are stripped
