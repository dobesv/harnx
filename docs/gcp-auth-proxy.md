# GCP Auth Injection (harnx-proxy-auth)

`harnx-proxy-auth` provides a transparent authentication proxy for Google Cloud Platform (GCP) services. It allows tools executed by agents (like the Node `@google-cloud/bigquery` library; see [sandbox-run.md](sandbox-run.md) for standalone sandbox execution) to authenticate using host-side credentials without ever seeing a reusable Google credential file or long-lived token.

## The Problem It Solves

Google Cloud client libraries typically use **Application Default Credentials (ADC)**, which look for a JSON key file at `GOOGLE_APPLICATION_CREDENTIALS` or in `~/.config/gcloud`.

In a secure execution environment:
1. **No secrets are mounted**: Mounting a service account key into the agent environment risks accidental leakage or reuse by untrusted code.
2. **ADC requires files or metadata endpoints**: Most GCP SDKs fail immediately if they cannot find a credential file or reach a GCE metadata server, making file-free auth difficult.
3. **Token Refresh**: Tokens expire every hour. A simple one-time environment injection isn't enough for long-running sessions.

## How It Works

`harnx-proxy-auth` solves this using **GCE metadata emulation** and a **resident proxy hook**:

1. **Metadata Emulation**: The proxy injects environment variables (`GCE_METADATA_HOST`, `METADATA_SERVER_DETECTION`) that redirect ADC discovery to the proxy itself. This is configured via a `--env` jaq flag that references `$proxy_port` (the proxy's runtime-assigned loopback port).
2. **Resident Hook**: A persistent Python script (`example_config/gcp-auth-hook.py`) runs alongside the proxy (loaded via the `--hook` flag). This script holds the real, host-minted Google token in memory and handles token refresh.
3. **Synthetic Responses**: When an SDK tool asks for a token from the "metadata server" (the proxy), the resident hook intercepts the request and returns a **synthetic response** (`.respond`) containing a placeholder token. This satisfies the SDK's ADC requirement without exposing the real token.
4. **Header Rewriting**: When the tool makes a real request to `*.googleapis.com`, the proxy intercepts the call and swaps the placeholder header for a real `Authorization: Bearer <token>` header before forwarding it to Google.

The real token exists only in the proxy process memory and on the wire between the proxy and Google. It never enters the agent or tool execution environment.

## Installation

The proxy is built from the `harnx-proxy-auth` crate:

```bash
cargo install --path crates/harnx-proxy-auth
```

## Modes of Operation

`harnx-proxy-auth` supports two deployment and execution models:

1. **Tool Server + Middleware Mode**: Standard in Harnx agent execution. Runs as a persistent tool service exposing `proxy_auth_environment`, paired with in-process middleware on bash tools.
2. **Standalone Sandbox Run (`harnx-sandbox-run`)**: Runs as a sidecar tool service over MCP stdio using `--hook environment <COMMAND> [ARGS...] \;`.

---

### 1. Tool Server + Middleware Mode

In modern Harnx workflows, `harnx-proxy-auth` runs as a long-lived tool service exposing the `proxy_auth_environment` tool.

#### Architecture & Co-location
Because the proxy binds to loopback (`127.0.0.1:<port>`) and injects CA certificates generated on the host, **the proxy tool service must be deployed in the same filesystem and network namespace as `harnx-bash-tools`**:
- **Managed Supervisor**: Harnx starts both tool services together on the bash host.
- **Unmanaged Deployment**: Operators deploy `harnx-proxy-auth --tool-server ...` alongside `harnx-bash-tools` on the bash host, connected to a common NATS cluster. The worker running agent turns and in-process middleware can be remote. When running multiple bash hosts or proxy instances, each pair must use distinct routing scopes (`HARNX_SERVER_SCOPE`) and package/config identities to avoid mixing local paths or loopback endpoints across hosts.

#### Tool Server Configuration (`tool_servers/proxy_auth.yaml`)

```yaml
command: harnx-proxy-auth
description: Long-lived GCP auth proxy tool service
args:
  - --tool-server
  - --env
  - '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present","GOOGLE_CLOUD_PROJECT":"my-project-id"}'
  - --hook
  - ./example_config/gcp-auth-hook.py
```

The tool service exposes the `proxy_auth` toolset with the `environment` tool (`proxy_auth_environment`). It returns:
```json
{
  "env": {
    "HTTP_PROXY": "http://127.0.0.1:4312",
    "HTTPS_PROXY": "http://127.0.0.1:4312",
    "SSL_CERT_FILE": "/tmp/ca.pem",
    "NODE_EXTRA_CA_CERTS": "/tmp/ca.pem",
    "CURL_CA_BUNDLE": "/tmp/ca.pem",
    "REQUESTS_CA_BUNDLE": "/tmp/ca.pem",
    "GIT_SSL_CAINFO": "/tmp/ca.pem",
    "GCE_METADATA_HOST": "127.0.0.1:4312",
    "METADATA_SERVER_DETECTION": "assume-present",
    "GOOGLE_CLOUD_PROJECT": "my-project-id"
  },
  "filesystem": {
    "ca_cert_path": "/tmp/ca.pem",
    "temp_file_root": null
  }
}
```

#### Bash Tool Middleware (`tool_servers/bash.yaml`)

Bash tools use in-process middleware to query `proxy_auth_environment` and merge the proxy configuration into the command's environment:

```yaml
middleware:
  - name: gcp-proxy-auth
    match: '^bash_(exec|spawn)$'
    python: |
      additions = tools.proxy_auth_environment()
      if additions.get("is_error") or additions.get("isError"):
          raise RuntimeError("proxy auth environment lookup failed")
      args = tool_input()["arguments"]
      caller_env = args.get("env")
      if not isinstance(caller_env, dict):
          caller_env = {}
      env = additions["env"]
      env.update(caller_env)  # Caller environment overrides take precedence
      args["env"] = env
      respond(call_next(args))
    tools: [proxy_auth_environment]
    timeout: 25h
    on_error: deny
```

---

### 2. Standalone Sandbox Run Usage

For standalone commands running in `harnx-sandbox-run`, `harnx-proxy-auth` runs directly as an MCP stdio tool service with hook type `environment`:

```bash
harnx-sandbox-run \
  --hook environment harnx-proxy-auth \
    --mcp-stdio \
    --name=proxy_auth \
    --enable-tool=environment \
    --env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present","GOOGLE_CLOUD_PROJECT":"my-project-id"}' \
    --hook ./example_config/gcp-auth-hook.py \
  \; \
  -- node my-bigquery-script.js
```

### JSONL Protocol for Resident Hooks

Resident hooks communicate with the proxy via JSONL on stdin/stdout:
- **Request (from proxy)**: `{"id": "...", "method": "...", "host": "...", "path": "...", "headers": {...}}`
- **Response (from hook)**:
    - `{"id": "...", "headers": {...}}`: Patch request headers.
    - `{"id": "...", "respond": {"status": 200, "headers": {...}, "body": "..."}}`: Return a synthetic response.
    - `{"id": "...", "block": true}`: Block the request (403).

### Advanced / Optional Host Environment Variables

`gcp-auth-hook.py` supports optional environment variables on the host:
- `HARNX_GCP_TOKEN_CMD`: Token source command (default: `gcloud auth print-access-token`).
- `HARNX_GCP_SA`: Service account for impersonation.
- Example: `HARNX_GCP_TOKEN_CMD='gcloud auth print-access-token --impersonate-service-account=$HARNX_GCP_SA'` enables service account impersonation and scoping without exposing reusable Google credentials in the tool execution environment.

## Validation (Manual E2E)

You can verify the setup with these steps:

1. **Host Setup**: Ensure you have `gcloud` authenticated on the host running the proxy:
   ```bash
   gcloud auth login
   ```
2. **Run Sandbox / Agent Session**:
   ```bash
   harnx-sandbox-run \
     --hook environment harnx-proxy-auth --mcp-stdio --name=proxy_auth --enable-tool=environment \
       --env '{"GCE_METADATA_HOST":"127.0.0.1:\($proxy_port)","METADATA_SERVER_DETECTION":"assume-present","GOOGLE_CLOUD_PROJECT":"my-project-id"}' \
       --hook ./example_config/gcp-auth-hook.py \
     \; \
     -- bash
   ```
3. **Inside Tool Execution**:
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

- **Namespace Isolation**: Each proxy instance uses a random loopback port. The environment variables pointing to this port are private to the bash host / sandbox.
- **No Credentials**: No `.json` keys or `~/.config/gcloud` files are needed inside the agent sandbox or bash container.
- **Placeholder Tokens**: The emulation layer returns only a placeholder token to the sandbox. The real token is attached only on egress to `*.googleapis.com`.
- **Precedence**: Caller-provided environment variables take precedence over proxy-injected variables, allowing manual overrides when explicitly passed.
- **Unix Only**: Shebang-based resident hooks are currently supported on Unix-like systems only.

## Limitations

- **Unix Only**: Shebang `--hook` stages require a Unix environment.
- **CA Bundle**: Tools must honor `HTTPS_PROXY` and the CA bundle injected by the proxy (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, etc.).
- **Token Refresh**: Token refresh is handled by the hook script (the provided `gcp-auth-hook.py` handles this automatically).

### `NO_PROXY` Hygiene

Do **not** add `googleapis.com` or `127.0.0.1` to `NO_PROXY`.
- The GCE metadata emulation relies on the tool talking to the proxy at `127.0.0.1`.
- Auth injection relies on the proxy intercepting calls to `googleapis.com`.
If these are in `NO_PROXY`, the tool will bypass the proxy and fail to authenticate.

## See Also

- [Hooks Guide](hooks-guide.md)
- [Tool Middleware Guide](middleware-guide.md)
- [harnx-sandbox-run](sandbox-run.md)
