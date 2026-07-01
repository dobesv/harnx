# GCP Auth Injection (harnx-proxy-auth)

`harnx-proxy-auth` provides a transparent authentication proxy for Google Cloud Platform (GCP) services. It allows tools inside a sandbox (like the Node `@google-cloud/bigquery` library; see [sandbox-run.md](sandbox-run.md) for general sandbox usage) to authenticate using host-side credentials without ever seeing a reusable Google credential file or long-lived token.

## The Problem It Solves

Google Cloud client libraries typically use **Application Default Credentials (ADC)**, which look for a JSON key file at `GOOGLE_APPLICATION_CREDENTIALS` or in `~/.config/gcloud`.

In a secure sandbox:
1. **No secrets are mounted**: Mounting a service account key into the sandbox risks accidental leakage or reuse by untrusted code.
2. **ADC requires files**: Most GCP SDKs fail immediately if they cannot find a credential file or reach a GCE metadata server, making file-free auth difficult.
3. **Token Refresh**: Tokens expire every hour. A simple one-time environment injection isn't enough for long-running sessions.

## How It Works

`harnx-proxy-auth` solves this using a **resident proxy hook** and **GCE metadata emulation**:

1. **Metadata Emulation**: The proxy supports injecting environment variables (`GCE_METADATA_HOST`, `METADATA_SERVER_DETECTION`) that redirect the sandbox's ADC discovery to the proxy itself. This is enabled by passing a `--env` flag that uses the `$proxy_port` variable (the proxy's runtime-assigned port).
2. **Resident Hook**: A persistent Python or shell script runs alongside the proxy (loaded via the `--hook` flag). This script holds the real, host-minted Google token in memory.
3. **Synthetic Responses**: When a tool in the sandbox asks for a token from the "metadata server" (the proxy), the resident hook intercepts the request and returns a **synthetic response** (`.respond`) containing a placeholder token. This satisfies the SDK's ADC requirement without exposing the real token.
4. **Header Rewriting**: When the tool makes a real request to `*.googleapis.com`, the proxy intercepts the call and swaps the placeholder header for a real `Authorization: Bearer <token>` header before forwarding it to Google.

The real token only exists in the proxy's memory and on the wire between the proxy and Google. It never enters the sandbox.

## Installation

The proxy is part of the `harnx-proxy-auth` crate:

```bash
cargo install --path crates/harnx-proxy-auth
```

## Usage

### CLI Flags

- `--env "JQ_FILTER"`: Injects environment variables into the sandbox. Use `$proxy_port` (a string) to reference the proxy's assigned port.
  - Example: `--env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present"}'`
  - To set the project ID: `--env '{"GOOGLE_CLOUD_PROJECT":"my-project-id"}'` (can be combined into one object).
- `--hook "HOOK"`: Attaches a hook stage. Accepted forms:
  - **Script path**: Starting with `/`, `./`, or `../`. Run directly from disk (relative to proxy CWD). Must exist and be executable; may be a shebang script or any other executable/binary.
  - **Inline script**: Content starting with `#!`. Proxy writes it to a temp file before launching, so inline form must include a shebang.
  - **Jaq filter**: Any other value is treated as a jaq filter.
- `--hook-timeout-secs <n>`: Sets the per-request timeout for exec `--hook` stages (default: 30s).

### Example: BigQuery Access

To run a Node.js script using BigQuery inside a sandbox:

```bash
harnx-sandbox-run \
  --hook harnx-proxy-auth \
    --env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present","GOOGLE_CLOUD_PROJECT":"<id>"}' \
    --hook ./example_config/gcp-auth-hook.py \
  \; \
  -- node my-bigquery-script.js
```

You can also combine multiple hooks:

```bash
harnx-proxy-auth \
  --env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present"}' \
  --hook ./example_config/gcp-auth-hook.py \
  --hook 'if .host == "my-internal-api.com" then .headers["X-Internal"] = "true" else . end' \
  -- ...
```

### JSONL Protocol

Resident hooks communicate with the proxy via JSONL on stdin/stdout.
- **Request (from proxy)**: `{"id": "...", "method": "...", "host": "...", "path": "...", "headers": {...}}`
- **Response (from hook)**:
    - `{"id": "...", "headers": {...}}`: Patch request headers.
    - `{"id": "...", "respond": {"status": 200, "headers": {...}, "body": "..."}}`: Return a synthetic response.
    - `{"id": "...", "block": true}`: Block the request (403).

## Validation (Manual E2E)

You can verify the setup with these steps:

1. **Host Setup**: Ensure you have `gcloud` authenticated.
2. **Run Sandbox**:
   ```bash
   harnx-sandbox-run \
     --hook harnx-proxy-auth \
       --env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present","GOOGLE_CLOUD_PROJECT":"<project-id>"}' \
       --hook ./example_config/gcp-auth-hook.py \
     \; \
     -- bash
   ```
3. **Inside Sandbox**:
   - Verify `GOOGLE_APPLICATION_CREDENTIALS` is unset.
   - Run a query (e.g., using `node`):
     ```js
     const {BigQuery} = require('@google-cloud/bigquery');
     const bq = new BigQuery();
     const [rows] = await bq.query({query: 'SELECT 1 AS ok'});
     console.log(rows);
     ```
4. **Logs**: Check proxy logs (or stderr) to see the metadata request being answered synthetically and the BigQuery request being forwarded with the rewritten header.

## Other Providers

The same resident hook mechanism is used for other providers:

- **GitHub App**: `example_config/github-app-auth-hook.py` mints and caches installation tokens, injecting them as Bearer tokens (API) or Basic auth (git).
- **Atlassian/Jira**: `example_config/jira-auth-hook.py` simplifies complex `acli` configurations into a single resident process.

## Security

- **Sandbox Isolation**: Each proxy instance uses a random loopback port. The environment variables pointing to this port are private to the sandbox.
- **No Credentials**: No `.json` keys or `~/.config/gcloud` files are needed in the sandbox.
- **Placeholder Tokens**: The emulation layer returns only a placeholder token to the sandbox. The real token is attached only on egress to `*.googleapis.com`.
- **Unix Only**: Shebang-based resident hooks are currently supported on Unix-like systems only.

## Limitations

- **Unix Only**: Shebang `--hook` stages require a Unix environment.
- **CA Bundle**: Tools must honor the `HTTPS_PROXY` and the CA bundle injected by the proxy.
- **Refresh**: Token refresh is handled by the hook script (the provided Python scripts handle this automatically).

### `NO_PROXY` Hygiene

Do **not** add `googleapis.com` or `127.0.0.1` to your `NO_PROXY` environment variable. 
- The GCE metadata emulation relies on the tool talking to the proxy at `127.0.0.1`.
- Auth injection relies on the proxy intercepting calls to `googleapis.com`.
If these are in `NO_PROXY`, the tool will bypass the proxy and fail to authenticate.

## Context

For more technical details on the implementation, see the solution note: [GCP Auth Injection via Resident Proxy Hooks](solutions/proxy-hooks/gcp-auth-injection-2026-07-01.md).

## See Also

- [AWS Credential Injection](aws-creds.md)
- [harnx-sandbox-run](sandbox-run.md)
