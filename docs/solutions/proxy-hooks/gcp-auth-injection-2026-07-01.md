---
title: "GCP auth injection via resident proxy hooks and GCE metadata emulation"
date: 2026-07-01
category: proxy-hooks
problem_type: integration_issue
component: harnx-proxy-auth
root_cause: "sandbox credential isolation vs GCP SDK Application Default Credentials (ADC) requirements"
resolution_type: code_fix
severity: medium
tags:
  - gcp
  - proxy
  - auth
  - bigquery
  - metadata
  - token
plan_ref: gcp-creds-helper
---

## Problem

Enabling Google Cloud Platform (GCP) SDKs (specifically Node.js `@google-cloud/bigquery`) to function inside a strictly isolated sandbox without mounting credential files or exposing reusable long-lived tokens. 

Standard GCP libraries require Application Default Credentials (ADC), which usually look for a local JSON file. If no file is found, they attempt to reach a GCE metadata server at a fixed IP. In a sandbox where network access is restricted to a MITM proxy, the libraries fail to authenticate.

## Solution

The `harnx-proxy-auth` component provides a unified hook mechanism and built-in metadata emulation to bridge this gap.

### Resident Proxy Hooks

The proxy allows attaching external executables via the `--hook` flag. When the flag points to an executable file path (e.g., `./example_config/gcp-auth-hook.py`), the proxy spawns it directly as a long-lived resident process. Path hooks only need execute permission; if they use a shebang, the OS honors it. Inline `#!...` hooks still materialize to temp files and therefore require a shebang.

- **Protocol**: Communication occurs via JSONL over stdin/stdout.
- **Request**: The proxy sends a JSON object containing request metadata (`id`, `method`, `host`, `path`, `headers`).
- **Response**: The hook returns a JSON object that can modify headers, block the request, or return a **synthetic response** (`.respond`).
- **Lifecycle**: The process is lazy-started, survives concurrent requests (via actor-based concurrency), and is automatically restarted if it crashes.

### Synthetic Responses (`.respond`)

The proxy was updated to support a `.respond` field in the hook output. If present, the proxy skips the upstream request and returns the specified status code, headers, and body directly to the client.

```json
{
  "id": "req-1",
  "respond": {
    "status": 200,
    "headers": {"content-type": "application/json", "metadata-flavor": "Google"},
    "body": "{\"access_token\": \"placeholder-token\", \"expires_in\": 3599}"
  }
}
```

### GCE Metadata Emulation

To bootstrap ADC without files, the proxy supports injecting environment variables into the sandbox via the `--env` flag. This uses a `$proxy_port` jaq variable to point to the proxy's runtime port:

1. `GCE_METADATA_HOST=127.0.0.1:\($proxy_port)`: Redirects metadata calls to the proxy.
2. `METADATA_SERVER_DETECTION=assume-present`: Skips the initial connectivity check that often fails in proxied environments.

Example:
`--env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present"}'`

The resident hook (`./example_config/gcp-auth-hook.py`) intercepts any request starting with `/computeMetadata/` and returns the synthetic JSON response expected by the Google SDKs.

### Header Rewriting (Bearer Tokens)

Unlike AWS SigV4, GCP authentication uses simple Bearer tokens. This allows the proxy to:
1. Provide a **placeholder token** to the sandbox via the metadata emulation.
2. Intercept outbound HTTPS calls to `*.googleapis.com`.
3. Swap the `Authorization: Bearer <placeholder>` header with a **real token** minted on the host side.

The real token is cached in the resident hook's memory and refreshed automatically on the host, ensuring the sandbox never sees it.

## Why This Works

1. **Isolation**: The sandbox never holds a reusable credential. The host-side token is only attached on egress.
2. **File-free**: No `.json` files need to be managed or mounted into the sandbox.
3. **Standard Support**: By emulating the metadata server protocol, we support any GCP SDK without modification.
4. **Scoping**: By using `gcloud auth print-access-token --impersonate-service-account=$SA` as the token source, the operator can granularly scope what the sandbox can access.

## Implementation Details

### Unified Stage Pipeline

The `TransformPipeline` (in `crates/harnx-proxy-auth/src/transform.rs`) applies hook stages in the exact order they appear on the CLI. 

```rust
pub enum Stage {
    Jaq { filter: Arc<CompiledFilter>, vars: Arc<JaqVars> },
    Exec(Arc<ExecHookProcess>),
}
```

This allows interleaving resident scripts and jaq filters. Sequential application (`stage1 -> stage2 -> stage3`) ensures predictable behavior.

### Host-side Token Minting

The `gcp-auth-hook.py` script uses the `HARNX_GCP_TOKEN_CMD` environment variable (defaulting to `gcloud auth print-access-token`) to fetch tokens. This keeps the token-fetching logic configurable and separate from the proxy's core.

## Prevention Strategies

- **Fail-safe Passthrough**: If a hook script hangs or crashes, the proxy times out (default 30s) and passes the request through unmodified. An unauthenticated request will fail at the Google API level, preventing silent failures or data leaks.
- **NO_PROXY Hygiene**: Documentation warns users not to put `googleapis.com` in `NO_PROXY`, as that would bypass the proxy and break auth injection.

## Related Issues

- **Plan**: `gcp-creds-helper`
- **GitHub Issue**: #593
- **Related Docs**: [gcp-auth-proxy.md](../../gcp-auth-proxy.md), [aws-creds.md](../../aws-creds.md)
- **Example Scripts**: [gcp-auth-hook.py](../../../example_config/gcp-auth-hook.py), [github-app-auth-hook.py](../../../example_config/github-app-auth-hook.py), [jira-auth-hook.py](../../../example_config/jira-auth-hook.py)
